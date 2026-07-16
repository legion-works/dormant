//! Shared IPC protocol types for dormant daemon ↔ CLI communication.
//!
//! Every request and response is a line-delimited JSON frame over a Unix
//! domain socket.  The types here are shared between `dormantd` (the server)
//! and `dormantctl` (the client) so the wire format has a single source of
//! truth.

use serde::{Deserialize, Serialize};

use crate::doctor::DoctorReport;
use crate::rules::{EmergencyWakeReport, ExerciseReport, StateSnapshot};

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
    /// Run the doctor: report live daemon health from owned state + active
    /// network-service probes.  Replied via [`IpcResponse::doctor_report`].
    Doctor,
    /// `dormantctl emergency-wake` — panic-recovery. Force-wake every
    /// display and pause every rule indefinitely, regardless of the
    /// per-display state machine's phase.  Replied with an
    /// [`EmergencyWakeReport`] via [`IpcResponse::emergency_report`].
    EmergencyWake,
    /// `dormantctl doctor --exercise <display>` — control-path
    /// verification. Run the blank → read → wake → read → restore sequence
    /// on the named display and reply with an [`ExerciseReport`] via
    /// [`IpcResponse::exercise_report`].
    Exercise {
        /// Display id to exercise.
        display: String,
    },
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
    /// A [`DoctorReport`] (present only for `Doctor` responses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctor_report: Option<DoctorReport>,
    /// An [`EmergencyWakeReport`] (present only for `EmergencyWake`
    /// responses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emergency_report: Option<EmergencyWakeReport>,
    /// An [`ExerciseReport`] (present only for `Exercise` responses).
    /// `serde(default) + skip_serializing_if` keeps the byte shape of
    /// every other response variant unchanged — pre-exercise clients see
    /// exactly the same JSON as before this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exercise_report: Option<ExerciseReport>,
}

impl IpcResponse {
    /// Build a success response with an optional snapshot.
    #[must_use]
    pub fn ok(snapshot: Option<StateSnapshot>) -> Self {
        Self {
            ok: true,
            error: None,
            snapshot,
            doctor_report: None,
            emergency_report: None,
            exercise_report: None,
        }
    }

    /// Build an error response.
    #[must_use]
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            snapshot: None,
            doctor_report: None,
            emergency_report: None,
            exercise_report: None,
        }
    }

    /// Build a response carrying a doctor report.
    #[must_use]
    pub fn doctor(report: DoctorReport) -> Self {
        Self {
            ok: true,
            error: None,
            snapshot: None,
            doctor_report: Some(report),
            emergency_report: None,
            exercise_report: None,
        }
    }

    /// Build a response carrying an emergency-wake report.
    #[must_use]
    pub fn emergency(report: EmergencyWakeReport) -> Self {
        Self {
            ok: true,
            error: None,
            snapshot: None,
            doctor_report: None,
            emergency_report: Some(report),
            exercise_report: None,
        }
    }

    /// Build a response carrying an exercise report.
    #[must_use]
    pub fn exercise(report: ExerciseReport) -> Self {
        Self {
            ok: true,
            error: None,
            snapshot: None,
            doctor_report: None,
            emergency_report: None,
            exercise_report: Some(report),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DisplayId;

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

    #[test]
    fn request_doctor_serde() {
        let req = IpcRequest::Doctor;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"doctor"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, IpcRequest::Doctor));
    }

    #[test]
    fn request_emergency_wake_serde() {
        let req = IpcRequest::EmergencyWake;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"emergency_wake"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, IpcRequest::EmergencyWake));
    }

    #[test]
    fn request_exercise_serde() {
        let req = IpcRequest::Exercise {
            display: "mon".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"exercise","display":"mon"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Exercise { display } => assert_eq!(display, "mon"),
            _ => panic!("expected Exercise"),
        }
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
        assert!(back.doctor_report.is_none());
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
            rollback: None,
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

    #[test]
    fn response_ok_no_doctor_report_stays_bare() {
        // Adding the doctor_report field must NOT add a key when None — the
        // existing `{"ok":true}` wire contract is preserved.
        let resp = IpcResponse::ok(None);
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true}"#);
        assert!(!json.contains("doctor_report"));
    }

    #[test]
    fn response_error_no_doctor_report_stays_bare() {
        let resp = IpcResponse::error("engine not available");
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":false,"error":"engine not available"}"#);
        assert!(!json.contains("doctor_report"));
    }

    #[test]
    fn response_doctor_serializes_report() {
        let report = DoctorReport { checks: vec![] };
        let resp = IpcResponse::doctor(report);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""doctor_report""#));
        assert!(!json.contains(r#""snapshot""#));
        let back: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert!(back.snapshot.is_none());
        assert!(back.doctor_report.is_some());
    }

    #[test]
    fn response_doctor_back_compat_old_daemon() {
        // An OLD daemon reply (no doctor_report key) must still parse under
        // the new client: the field is serde-default + skip_serializing_if.
        let json = r#"{"ok":true,"snapshot":null}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.doctor_report.is_none());
    }

    #[test]
    fn response_emergency_serializes_report() {
        // Verify the wire shape: emergency_wake responses carry an
        // emergency_report object keyed "emergency_report", and the other
        // slots are absent.
        let report = EmergencyWakeReport {
            paused: true,
            displays: vec![],
        };
        let resp = IpcResponse::emergency(report);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""emergency_report""#));
        assert!(!json.contains(r#""snapshot""#));
        assert!(!json.contains(r#""doctor_report""#));
        let back: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert!(back.snapshot.is_none());
        assert!(back.doctor_report.is_none());
        assert!(back.emergency_report.is_some());
    }

    #[test]
    fn response_emergency_back_compat_pre_emergency_daemon() {
        // A pre-emergency daemon reply has no emergency_report key — the new
        // client must still parse it (serde-default + skip_serializing_if).
        let json = r#"{"ok":true,"snapshot":null}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.emergency_report.is_none());
    }

    #[test]
    fn response_emergency_omits_field_when_none() {
        // `IpcResponse::ok(None)` must keep its byte-identical wire shape so
        // old clients see exactly the same JSON as before the field was
        // added. The skip_serializing_if guard enforces this.
        let resp = IpcResponse::ok(None);
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true}"#);
        assert!(!json.contains("emergency_report"));
    }

    #[test]
    fn response_exercise_serializes_report() {
        // Verify the wire shape: exercise responses carry an
        // exercise_report object keyed "exercise_report", and the other
        // slots are absent.
        let report = crate::rules::ExerciseReport {
            display: DisplayId("mon".into()),
            pre_phase: "active".into(),
            paused_rules: vec![],
            steps: vec![],
        };
        let resp = IpcResponse::exercise(report);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""exercise_report""#));
        assert!(!json.contains(r#""snapshot""#));
        assert!(!json.contains(r#""doctor_report""#));
        assert!(!json.contains(r#""emergency_report""#));
        let back: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert!(back.snapshot.is_none());
        assert!(back.doctor_report.is_none());
        assert!(back.emergency_report.is_none());
        assert!(back.exercise_report.is_some());
    }

    #[test]
    fn response_exercise_back_compat_pre_exercise_daemon() {
        // A pre-exercise daemon reply has no exercise_report key — the
        // new client must still parse it (serde-default +
        // skip_serializing_if).
        let json = r#"{"ok":true,"snapshot":null}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.exercise_report.is_none());
    }

    #[test]
    fn response_exercise_omits_field_when_none() {
        // `IpcResponse::ok(None)` must keep its byte-identical wire shape
        // so old clients see exactly the same JSON as before the field
        // was added. The skip_serializing_if guard enforces this.
        let resp = IpcResponse::ok(None);
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true}"#);
        assert!(!json.contains("exercise_report"));
    }

    #[test]
    fn response_exercise_does_not_perturb_other_variants() {
        // The doctor / emergency / ok variants must keep their existing
        // JSON shapes after the exercise_report field is added — the
        // back-compat contract for every pre-exercise client.
        assert!(
            !serde_json::to_string(&IpcResponse::doctor(crate::doctor::DoctorReport {
                checks: vec![]
            }))
            .unwrap()
            .contains("exercise_report")
        );
        assert!(
            !serde_json::to_string(&IpcResponse::emergency(crate::rules::EmergencyWakeReport {
                paused: true,
                displays: vec![],
            }))
            .unwrap()
            .contains("exercise_report")
        );
    }
}
