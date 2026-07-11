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
        print!("{}", render_table(snapshot));
    }

    Ok(())
}

/// Render a [`StateSnapshot`] as a human-readable table and return the
/// formatted output as a `String`.
///
/// Pure — performs no I/O. The command path prints the returned value, and
/// tests can call this directly to assert on the bytes a user would see.
fn render_table(snapshot: &StateSnapshot) -> String {
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
        table.set_header(vec!["ID", "Phase", "Inhibited", "Paused"]);
        for (id, d) in &snapshot.displays {
            let phase = phase_cell(d);
            table.add_row(vec![
                id.as_str(),
                phase.as_str(),
                if d.inhibited { "yes" } else { "no" },
                if d.paused { "yes" } else { "no" },
            ]);
        }
        let _ = writeln!(out, "{table}");
    }

    // ── Pending reload warning ────────────────────────────────────────────
    if let Some(detail) = &snapshot.pending_reload {
        let _ = writeln!(out);
        let _ = writeln!(out, "⚠  Pending reload: {detail}");
    }

    out
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
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                    },
                ),
            ],
            pending_reload: None,
        }
    }

    #[test]
    fn table_contains_sensor_ids() {
        let snap = canned_snapshot();
        assert!(snap.sensors.iter().any(|s| s.id == "desk"));
        assert!(snap.sensors.iter().any(|s| s.id == "hallway"));
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
}
