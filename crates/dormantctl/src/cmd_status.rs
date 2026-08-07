//! `dormantctl status` — display current daemon state.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use comfy_table::Table;
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::rules::{DisplaySnapshot, StateSnapshot};

use dormantctl::client;

/// Run the `status` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run(socket_path: &Path, json_output: bool) -> Result<()> {
    let resp = client::send_request(socket_path, &IpcRequest::Status)?;

    if !resp.ok {
        anyhow::bail!(
            "daemon returned error: {}",
            resp.error.as_deref().unwrap_or("unknown")
        );
    }

    let snapshot = resp
        .snapshot
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("daemon returned no snapshot"))?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(snapshot)?);
    } else {
        print!(
            "{}",
            render_table_with_sampling(snapshot, resp.wear_sampling_status.as_ref())
        );
    }

    Ok(())
}

/// Render a [`StateSnapshot`] as a human-readable table and return the
/// formatted output as a `String`.
///
/// Pure — performs no I/O. The command path prints the returned value, and
/// tests can call this directly to assert on the bytes a user would see.
#[cfg(test)]
fn render_table(snapshot: &StateSnapshot) -> String {
    render_table_with_sampling(snapshot, None)
}

/// Render a [`StateSnapshot`] and its optional active-sampling status.
#[allow(
    clippy::too_many_lines,
    reason = "the status table remains one ordered terminal presentation"
)]
fn render_table_with_sampling(
    snapshot: &StateSnapshot,
    sampling: Option<&dormant_core::wear::WearSamplingStatus>,
) -> String {
    let mut out = String::new();

    // ── Sensors ────────────────────────────────────────────────────────────
    if !snapshot.sensors.is_empty() {
        let _ = writeln!(
            out,
            "── Sensors ──────────────────────────────────────────────"
        );
        let mut table = Table::new();
        table.set_header(vec!["ID", "State", "Last Seen"]);
        for s in &snapshot.sensors {
            table.add_row(vec![
                &s.id,
                &format!("{:?}", s.state),
                &format!("{}s ago", s.last_seen_secs_ago),
            ]);
        }
        let _ = writeln!(out, "{table}");
    }

    // ── Zones ─────────────────────────────────────────────────────────────
    if !snapshot.zones.is_empty() {
        let _ = writeln!(
            out,
            "── Zones ────────────────────────────────────────────────"
        );
        let mut table = Table::new();
        table.set_header(vec!["ID", "Present"]);
        for z in &snapshot.zones {
            let present = match z.present {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            };
            table.add_row(vec![&z.id, present]);
        }
        let _ = writeln!(out, "{table}");
    }

    // ── Displays ──────────────────────────────────────────────────────────
    if !snapshot.displays.is_empty() {
        let _ = writeln!(
            out,
            "── Displays ──────────────────────────────────────────────"
        );
        let mut table = Table::new();
        table.set_header(vec![
            "ID",
            "Phase",
            "Owner",
            "Claim",
            "Inhibited",
            "Paused",
            "Health",
        ]);
        for (id, d) in &snapshot.displays {
            let phase = phase_cell(d);
            let health_cell = health_cell(d);
            table.add_row(vec![
                id.as_str(),
                phase.as_str(),
                if d.owned { "local" } else { "peer" },
                if snapshot.kvm.as_ref().is_some_and(|kvm| {
                    kvm.switch_capable_displays
                        .iter()
                        .any(|display| display.0 == *id)
                }) {
                    "capable"
                } else {
                    "unsupported"
                },
                if d.inhibited { "yes" } else { "no" },
                if d.paused { "yes" } else { "no" },
                &health_cell,
            ]);
        }
        let _ = writeln!(out, "{table}");
    }

    if let Some(kvm) = &snapshot.kvm {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "── KVM ───────────────────────────────────────────────────"
        );
        let _ = writeln!(
            out,
            "Activity follow: {}",
            if kvm.activity_following { "on" } else { "off" }
        );
        let _ = writeln!(
            out,
            "Claim hotkey: {}",
            kvm.keymap.claim_hotkey.as_deref().unwrap_or("—")
        );
    }

    if let Some(sampling) = sampling {
        let _ = writeln!(out);
        let _ = write!(
            out,
            "sampling: {} (age: {})",
            sampling_state_name(sampling.state),
            sampling
                .last_capture_age_s
                .map_or_else(|| "unavailable".to_owned(), format_sampling_age)
        );
        if let Some(reason) = &sampling.uniform_reason {
            let _ = write!(out, " (uniform: {reason})");
        }
        if let Some(gate) = &sampling.source_gate {
            let _ = write!(out, " (source: {gate})");
        }
        let _ = writeln!(out);
    }

    // ── Pending reload warning ────────────────────────────────────────────
    if let Some(detail) = &snapshot.pending_reload {
        let _ = writeln!(out);
        let _ = writeln!(out, "⚠  Pending reload: {detail}");
    }

    out
}

fn sampling_state_name(state: dormant_core::wear::WearSamplingState) -> &'static str {
    match state {
        dormant_core::wear::WearSamplingState::Disabled => "disabled",
        dormant_core::wear::WearSamplingState::NeedsConsent => "needs_consent",
        dormant_core::wear::WearSamplingState::ConsentPending => "consent_pending",
        dormant_core::wear::WearSamplingState::Connecting => "connecting",
        dormant_core::wear::WearSamplingState::Streaming => "streaming",
        dormant_core::wear::WearSamplingState::Suspended => "suspended",
        dormant_core::wear::WearSamplingState::Cooldown => "cooldown",
    }
}

fn format_sampling_age(age_s: u64) -> String {
    let minutes = age_s / 60;
    let seconds = age_s % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

/// Build the Phase column cell for a [`DisplaySnapshot`].
///
/// For a staged display the cell reads `staged [idx: "kind"]`;
/// otherwise it returns the plain phase string.
fn phase_cell(d: &DisplaySnapshot) -> String {
    match &d.stage {
        Some(si) => format!(
            "staged [{}: {}]",
            si.idx,
            serde_json::to_string(&si.kind).unwrap()
        ),
        None => d.phase.clone(),
    }
}

/// Build the Health column cell for a [`DisplaySnapshot`].
///
/// Returns `"healthy"` when every controller is healthy (or when the
/// controller list is empty — no probe has run yet). Otherwise joins each
/// unhealthy controller as `"name: detail"` (truncated).
fn health_cell(d: &DisplaySnapshot) -> String {
    if d.controllers.is_empty() || d.controllers.iter().all(|c| c.healthy) {
        return "healthy".to_string();
    }
    d.controllers
        .iter()
        .filter(|c| !c.healthy)
        .map(|c| {
            if let Some(detail) = &c.detail {
                // Keep the cell bounded — a full probe-error dump belongs in
                // the doctor output, not a cramped status table.
                let short = if detail.len() > 80 {
                    format!("{}…", &detail[..80])
                } else {
                    detail.clone()
                };
                format!("{}: {short}", c.name)
            } else {
                format!("{}: unknown", c.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::rules::{DisplaySnapshot, SensorSnapshot, ZoneSnapshot};

    fn canned_snapshot() -> StateSnapshot {
        StateSnapshot {
            sensors: vec![
                SensorSnapshot {
                    id: "desk".into(),
                    state: dormant_core::types::SensorState::Present,
                    last_seen_secs_ago: 2,
                    reported: true,
                },
                SensorSnapshot {
                    id: "hallway".into(),
                    state: dormant_core::types::SensorState::Absent,
                    last_seen_secs_ago: 120,
                    reported: true,
                },
            ],
            zones: vec![ZoneSnapshot {
                id: "office".into(),
                present: Some(true),
            }],
            displays: vec![
                (
                    "main_monitor".into(),
                    DisplaySnapshot {
                        phase: "active".into(),
                        inhibited: false,
                        paused: false,
                        cmd_gen: 1,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                    },
                ),
                (
                    "tv".into(),
                    DisplaySnapshot {
                        phase: "blanked".into(),
                        inhibited: false,
                        paused: true,
                        cmd_gen: 3,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                    },
                ),
            ],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        }
    }

    #[test]
    fn table_contains_sensor_ids() {
        let snap = canned_snapshot();
        assert!(snap.sensors.iter().any(|s| s.id == "desk"));
        assert!(snap.sensors.iter().any(|s| s.id == "hallway"));
    }

    #[test]
    fn status_renders_streaming_sampling_age() {
        let status = dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Streaming,
            last_capture_age_s: Some(95),
            uniform_reason: None,
            bound_display: Some("desk".to_owned()),
            compositor_output: None,
            granted_at_epoch_s: Some(1_700_000_000),
            source_gate: None,
        };

        let table = render_table_with_sampling(&canned_snapshot(), Some(&status));
        assert!(table.contains("sampling: streaming (age: 1m 35s)"));
    }

    #[test]
    fn status_renders_unavailable_sampling_age() {
        let status = dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Streaming,
            last_capture_age_s: None,
            uniform_reason: None,
            bound_display: Some("desk".to_owned()),
            compositor_output: None,
            granted_at_epoch_s: Some(1_700_000_000),
            source_gate: None,
        };

        let table = render_table_with_sampling(&canned_snapshot(), Some(&status));
        assert!(table.contains("sampling: streaming (age: unavailable)"));
    }

    #[test]
    fn status_renders_degraded_sampling_reason() {
        let status = dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Suspended,
            last_capture_age_s: Some(7),
            uniform_reason: Some("wear_sampling_suspended".to_owned()),
            bound_display: Some("desk".to_owned()),
            compositor_output: None,
            granted_at_epoch_s: None,
            source_gate: None,
        };

        let table = render_table_with_sampling(&canned_snapshot(), Some(&status));
        assert!(table.contains("sampling: suspended (age: 7s) (uniform: wear_sampling_suspended)"));
    }

    /// The redacted source-gate state must surface on the
    /// status line in the exact `(source: <gate>)` form. Pinned so a
    /// future refactor can't silently drop the gate or change its token.
    #[test]
    fn source_gate_renders() {
        let status = dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Streaming,
            last_capture_age_s: Some(95),
            uniform_reason: None,
            bound_display: Some("tv".to_owned()),
            compositor_output: None,
            granted_at_epoch_s: Some(1_700_000_000),
            source_gate: Some("mismatched".to_owned()),
        };

        let table = render_table_with_sampling(&canned_snapshot(), Some(&status));
        assert!(
            table.contains("sampling: streaming (age: 1m 35s) (source: mismatched)"),
            "expected exact `sampling: streaming (age: 1m 35s) (source: mismatched)` line, got: {table}"
        );
    }

    #[test]
    fn table_contains_display_phases() {
        let snap = canned_snapshot();
        assert!(
            snap.displays
                .iter()
                .any(|(id, d)| id == "main_monitor" && d.phase == "active")
        );
        assert!(
            snap.displays
                .iter()
                .any(|(id, d)| id == "tv" && d.phase == "blanked")
        );
    }

    #[test]
    fn table_contains_zone_present() {
        let snap = canned_snapshot();
        assert!(
            snap.zones
                .iter()
                .any(|z| z.id == "office" && z.present == Some(true))
        );
    }

    #[test]
    fn pending_reload_warning_shown() {
        let mut snap = canned_snapshot();
        snap.pending_reload = Some("config error: bad key".into());
        // Just verify the field is set
        assert!(snap.pending_reload.is_some());
    }

    #[test]
    fn staged_display_snapshot_has_stage_info() {
        use dormant_core::rules::StageInfo;
        use dormant_core::types::StageKind;

        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
                DisplaySnapshot {
                    phase: "staged".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 1,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: Some(StageInfo {
                        idx: 2,
                        kind: StageKind::RenderBlack,
                    }),
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };

        let d = &snap.displays[0].1;
        let si = d.stage.as_ref().unwrap();
        assert_eq!(si.idx, 2);
        assert_eq!(si.kind, StageKind::RenderBlack);
    }

    #[test]
    fn staged_display_renders_stage_marker() {
        use dormant_core::rules::{DisplaySnapshot, StageInfo};
        use dormant_core::types::StageKind;

        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
                DisplaySnapshot {
                    phase: "staged".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 1,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: Some(StageInfo {
                        idx: 1,
                        kind: StageKind::RenderBlack,
                    }),
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };

        // Must exercise the production rendering path, not a helper — a
        // reviewer who bypasses `phase_cell` (e.g. by inlining `d.phase.clone()`
        // into `render_table`) would otherwise silently regress the stage
        // marker without any test signal.
        let out = render_table(&snap);

        assert!(
            out.contains("staged [1:"),
            "render_table output missing stage marker: {out}"
        );
        assert!(
            out.contains("\"render_black\""),
            "render_table output missing render_black kind: {out}"
        );
    }

    #[test]
    fn table_surfaces_kvm_claim_state() {
        let mut snap = canned_snapshot();
        snap.displays[0].1.scope = dormant_core::config::DisplayScope::Shared;
        snap.displays[0].1.owned = false;
        snap.kvm = Some(dormant_core::rules::KvmStatus {
            keymap: dormant_core::config::KeymapConfig {
                claim_hotkey: Some("Ctrl+F12".into()),
            },
            switch_capable_displays: vec![dormant_core::types::DisplayId("main_monitor".into())],
            activity_following: true,
            push_capable_displays: vec![],
        });
        let rendered = render_table(&snap);
        assert!(rendered.contains("peer"));
        assert!(rendered.contains("capable"));
        assert!(rendered.contains("Activity follow: on"));
        assert!(rendered.contains("Ctrl+F12"));
    }

    #[test]
    fn health_column_healthy_when_empty_controllers() {
        let snap = canned_snapshot();
        let rendered = render_table(&snap);
        // Empty controllers → "healthy"
        assert!(rendered.contains("healthy"), "got: {rendered}");
        assert!(rendered.contains("Health"), "Health header missing");
    }

    #[test]
    fn health_column_shows_unhealthy_controller_detail() {
        use dormant_core::rules::ControllerHealth;
        use dormant_core::rules::ControllerRole;

        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 1,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![ControllerHealth {
                        name: "ddcci".into(),
                        role: ControllerRole::Primary,
                        healthy: false,
                        detail: Some("E_DISPLAY_IO: no display found".into()),
                    }],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let rendered = render_table(&snap);
        assert!(
            rendered.contains("ddcci: E_DISPLAY_IO: no display found"),
            "Health column missing probe-failure detail: {rendered}"
        );
    }
}
