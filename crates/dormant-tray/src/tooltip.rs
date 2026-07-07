//! Pure-logic tooltip construction.
//!
//! Tooltip shape: a single-line status header followed by a per-display
//! detail line joined with ` · `.  Examples:
//!
//! - `dormant — 2 displays, 1 paused` (header)
//! - `main: active · tv: blanked`     (detail)
//!
//! When the IPC is unreachable the caller feeds [`TooltipInputs::unreachable`]
//! and the function emits a single "dormant: daemon unreachable" line.

use dormant_core::rules::StateSnapshot;

/// Inputs the tooltip builder needs from the runtime.  The runtime owns
/// reachability because [`StateSnapshot`] does not carry it; the builder
/// stays pure.
#[derive(Debug, Clone)]
pub struct TooltipInputs<'a> {
    /// The current snapshot, if any.  `None` means the IPC loop hasn't
    /// received a snapshot yet (typically only during startup).
    pub snapshot: Option<&'a StateSnapshot>,
    /// Whether the IPC loop currently reports the daemon as unreachable.
    pub unreachable: bool,
}

/// A rendered tooltip, ready to hand to `ksni::Tray::title`/`tooltip`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tooltip {
    /// The headline (e.g. "dormant — 2 displays, 1 paused").
    pub title: String,
    /// The per-display detail line (e.g. "main: active · tv: blanked").
    /// Empty when there are no displays or the daemon is unreachable.
    pub body: String,
}

/// Build the tray tooltip from the current state.
///
/// Pure — performs no I/O.
#[must_use]
pub fn build_tooltip(inputs: &TooltipInputs<'_>) -> Tooltip {
    if inputs.unreachable {
        return Tooltip {
            title: "dormant: daemon unreachable".to_string(),
            body: String::new(),
        };
    }

    let Some(snap) = inputs.snapshot else {
        return Tooltip {
            title: "dormant: starting…".to_string(),
            body: String::new(),
        };
    };

    let n = snap.displays.len();
    let paused_count = snap.displays.iter().filter(|(_id, d)| d.paused).count();
    let header = match n {
        0 => "dormant — no displays configured".to_string(),
        1 => {
            let paused = if paused_count == 1 { ", paused" } else { "" };
            format!("dormant — 1 display{paused}")
        }
        n => {
            let paused = if paused_count > 0 {
                format!(", {paused_count} paused")
            } else {
                String::new()
            };
            format!("dormant — {n} displays{paused}")
        }
    };

    let mut parts: Vec<String> = Vec::with_capacity(snap.displays.len());
    for (id, d) in &snap.displays {
        // Phase + paused flag, e.g. `tv: blanked (paused)`.
        let phase = if d.paused {
            format!("{} (paused)", d.phase)
        } else {
            d.phase.clone()
        };
        parts.push(format!("{id}: {phase}"));
    }
    let body = parts.join(" · ");

    Tooltip {
        title: header,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::rules::{DisplaySnapshot, StateSnapshot};

    fn snap_two_displays() -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![
                (
                    "main".into(),
                    DisplaySnapshot {
                        phase: "active".into(),
                        inhibited: false,
                        paused: false,
                        cmd_gen: 0,
                        controllers: vec![],
                        stage: None,
                    },
                ),
                (
                    "tv".into(),
                    DisplaySnapshot {
                        phase: "blanked".into(),
                        inhibited: false,
                        paused: true,
                        cmd_gen: 0,
                        controllers: vec![],
                        stage: None,
                    },
                ),
            ],
            pending_reload: None,
        }
    }

    #[test]
    fn unreachable_short_circuits() {
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap_two_displays()),
            unreachable: true,
        });
        assert_eq!(t.title, "dormant: daemon unreachable");
        assert!(t.body.is_empty());
    }

    #[test]
    fn starting_state_with_no_snapshot() {
        let t = build_tooltip(&TooltipInputs {
            snapshot: None,
            unreachable: false,
        });
        assert_eq!(t.title, "dormant: starting…");
        assert!(t.body.is_empty());
    }

    #[test]
    fn header_counts_displays_and_paused() {
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap_two_displays()),
            unreachable: false,
        });
        assert_eq!(t.title, "dormant — 2 displays, 1 paused");
    }

    #[test]
    fn body_lists_per_display_phase() {
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap_two_displays()),
            unreachable: false,
        });
        assert_eq!(t.body, "main: active · tv: blanked (paused)");
    }

    #[test]
    fn empty_displays_announces_zero() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(t.title, "dormant — no displays configured");
        assert!(t.body.is_empty());
    }

    #[test]
    fn single_active_display_no_paused_marker() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    controllers: vec![],
                    stage: None,
                },
            )],
            pending_reload: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(t.title, "dormant — 1 display");
        assert_eq!(t.body, "mon: active");
    }
}
