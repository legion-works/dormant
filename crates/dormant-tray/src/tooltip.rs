//! Pure-logic tooltip construction.
//!
//! Tooltip shape: a single-line status header followed by the phase
//! glyph legend and a per-display detail line joined with ` · `.
//! Examples:
//!
//! - `dormant — 2 displays, 1 paused`               (header)
//! - `● active · ◐ staged · ○ blanked · ⚠ unreachable` (legend)
//! - `main: active · tv: blanked`                    (detail)
//!
//! The legend is what makes the per-display detail line self-documenting:
//! the same glyphs appear in the per-display submenu labels, so the
//! operator sees the same vocabulary in two places and the tooltip
//! answers "what does this circle mean" without leaving the tray.
//!
//! When the IPC is unreachable the caller feeds
//! [`TooltipInputs::unreachable`] and the function emits a single
//! "dormant: daemon unreachable" line.

use std::fmt::Write as _;

use dormant_core::rules::StateSnapshot;

/// The legend block the tooltip carries below the header.  Kept
/// inline so the renderer stays pure; matches the format
/// `crate::menu::submenu_label` uses for per-display submenu
/// labels, and `crate::icon::draw_pause_badge` for the tray icon
/// variant.
const PHASE_GLYPH_LEGEND: &str = "● active · ◐ staged · ○ blanked · ⚠ unreachable";

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
    /// The combined legend + per-display detail (e.g. "● active · ◐
    /// staged · ○ blanked · ⚠ unreachable\nmain: active · tv: blanked").
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
        let mut phase = if d.paused {
            format!("{} (paused)", d.phase)
        } else {
            d.phase.clone()
        };
        // Failure detail — wake attempts outrank a stale blank failure
        // (a display that's currently retrying wake is the more urgent
        // signal); each is mutually exclusive with the other in the
        // suffix so the tooltip line stays short.
        if d.wake_attempts > 0 {
            let _ = write!(phase, " (wake failing ×{})", d.wake_attempts);
        } else if d.last_blank_failed {
            phase.push_str(" (last blank failed)");
        }
        // Coverage marker — a display that no rule drives is never blanked
        // on its own, so the operator gets a plain "no automatic rule"
        // suffix on its line. Displays with at least one driving rule keep
        // the existing compact line; this only adds bytes to the
        // uncommon-case (manual-only) display.
        if d.rules.is_empty() {
            phase.push_str(" (no automatic rule \u{2014} dormant won't blank it on its own)");
        }
        parts.push(format!("{id}: {phase}"));
    }
    let detail = parts.join(" · ");

    // Body is the legend on its own line followed by the per-display
    // detail; an empty detail (no displays yet) collapses to just the
    // legend so the operator always sees the vocabulary.
    let body = if detail.is_empty() {
        PHASE_GLYPH_LEGEND.to_string()
    } else {
        format!("{PHASE_GLYPH_LEGEND}\n{detail}")
    };

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
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                        rules: vec!["office".into()],
                    },
                ),
                (
                    "tv".into(),
                    DisplaySnapshot {
                        phase: "blanked".into(),
                        inhibited: false,
                        paused: true,
                        cmd_gen: 0,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                        rules: vec!["office".into()],
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
    fn body_lists_per_display_phase_below_glyph_legend() {
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap_two_displays()),
            unreachable: false,
        });
        assert_eq!(
            t.body,
            "● active · ◐ staged · ○ blanked · ⚠ unreachable\nmain: active · tv: blanked (paused)"
        );
    }

    #[test]
    fn empty_displays_still_carries_legend() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(t.title, "dormant — no displays configured");
        // Even with no displays the operator still sees the glyph legend
        // (collapsed to a single line because the detail line is empty).
        assert_eq!(t.body, "● active · ◐ staged · ○ blanked · ⚠ unreachable");
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
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                    rules: vec!["desk".into()],
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(t.title, "dormant — 1 display");
        assert_eq!(
            t.body,
            "● active · ◐ staged · ○ blanked · ⚠ unreachable\nmon: active"
        );
    }

    #[test]
    fn wake_failing_display_gets_attempt_count_suffix() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
                DisplaySnapshot {
                    phase: "blanked".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 3,
                    last_blank_failed: false,
                    stage: None,
                    rules: vec!["desk".into()],
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(
            t.body,
            "● active · ◐ staged · ○ blanked · ⚠ unreachable\nmon: blanked (wake failing ×3)"
        );
    }

    #[test]
    fn blank_failed_display_gets_last_blank_failed_suffix() {
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
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: true,
                    stage: None,
                    rules: vec!["desk".into()],
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(
            t.body,
            "● active · ◐ staged · ○ blanked · ⚠ unreachable\nmon: active (last blank failed)"
        );
    }

    #[test]
    fn paused_and_wake_failing_combines_both_suffixes() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
                DisplaySnapshot {
                    phase: "blanked".into(),
                    inhibited: false,
                    paused: true,
                    cmd_gen: 0,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 2,
                    last_blank_failed: false,
                    stage: None,
                    rules: vec!["desk".into()],
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(
            t.body,
            "● active · ◐ staged · ○ blanked · ⚠ unreachable\nmon: blanked (paused) (wake failing ×2)"
        );
    }

    /// A display with at least one driving rule (`rules` non-empty) keeps
    /// its compact line — the operator already knows dormant will blank it.
    #[test]
    fn displays_with_rules_hide_coverage_marker() {
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
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                    rules: vec!["desk".into()],
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert_eq!(
            t.body,
            "● active · ◐ staged · ○ blanked · ⚠ unreachable\nmon: active"
        );
        assert!(
            !t.body.contains("no automatic rule"),
            "covered displays must NOT carry the coverage marker, got: {}",
            t.body
        );
    }

    /// A display with `rules == []` (manual-only) carries an explicit
    /// `no automatic rule` marker on its line so the operator knows
    /// dormant will never blank it on its own.
    #[test]
    fn displays_without_rules_show_no_coverage_marker() {
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
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                    rules: vec![],
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert!(
            t.body.contains("no automatic rule"),
            "uncovered displays must carry the coverage marker, got: {}",
            t.body
        );
        assert!(
            t.body.contains("dormant won't blank it on its own"),
            "marker must include the full explanation, got: {}",
            t.body
        );
    }

    /// A mixed snapshot carries the marker on the manual-only line and
    /// keeps the covered line compact.
    #[test]
    fn mixed_snapshot_marks_only_uncovered_displays() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![
                (
                    "mon".into(),
                    DisplaySnapshot {
                        phase: "active".into(),
                        inhibited: false,
                        paused: false,
                        cmd_gen: 0,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                        rules: vec!["desk".into()],
                    },
                ),
                (
                    "tv".into(),
                    DisplaySnapshot {
                        phase: "blanked".into(),
                        inhibited: false,
                        paused: false,
                        cmd_gen: 0,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                        rules: vec![],
                    },
                ),
            ],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        };
        let t = build_tooltip(&TooltipInputs {
            snapshot: Some(&snap),
            unreachable: false,
        });
        assert!(
            t.body.contains("mon: active"),
            "covered display has no marker, got: {}",
            t.body
        );
        assert!(
            t.body.contains("tv: blanked (no automatic rule"),
            "uncovered display carries the marker, got: {}",
            t.body
        );
        // Exactly one marker (on `tv`), not two.
        assert_eq!(
            t.body.matches("no automatic rule").count(),
            1,
            "exactly one marker expected, got: {}",
            t.body
        );
    }
}
