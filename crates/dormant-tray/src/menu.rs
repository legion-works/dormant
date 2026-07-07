//! Pure-logic menu model construction.
//!
//! The tray's menu is a fixed top-level layout (pause/resume + global
//! blank/wake + per-display submenus + web UI + quit) with **per-display
//! submenus** driven by the live [`StateSnapshot`].  Each display becomes
//! a submenu labeled `"<display-id> — <phase>"` (e.g. `"monitor — active"`)
//! containing `Blank now` / `Wake now` entries targeting that specific
//! display via [`Action::BlankOne`] / [`Action::WakeOne`].
//!
//! The menu rebuilds whenever the IPC loop applies a new snapshot — the
//! tray's `menu()` callback hands `ksni` the freshly-built `Vec<MenuEntry>`
//! on every refresh, so displays that disappear across a daemon reload
//! vanish from the menu without any explicit cleanup.

use std::time::Duration;

use dormant_core::rules::StateSnapshot;

/// One top-level menu entry.  Submenus recurse via [`MenuEntry::Submenu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// A clickable leaf item.
    Action {
        /// The visible label.
        label: String,
        /// Whether the entry is currently clickable.
        enabled: bool,
        /// The action to dispatch on click.
        action: Action,
    },
    /// A visual separator (no click target).
    Separator,
    /// A submenu containing more entries.
    Submenu {
        /// The visible label (e.g. `"monitor — active"`).
        label: String,
        /// Whether the submenu's child actions are enabled.  The submenu
        /// itself is always openable so the operator can still see why
        /// blank/wake are greyed out.
        enabled: bool,
        /// The entries inside the submenu (typically `Blank now` /
        /// `Wake now` for one display).
        entries: Vec<MenuEntry>,
    },
}

/// The action a leaf menu entry carries.  Pure data — no closures, no
/// D-Bus handles — so the model can be cloned, hashed, and tested in
/// isolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A visual separator (carried inside [`MenuEntry::Separator`] only —
    /// never appears inside an `Action` variant).
    Separator,
    /// Pause blanking for an optional duration (`None` ⇒ pause until resumed).
    Pause(Option<Duration>),
    /// Resume blanking.
    Resume,
    /// Force-blank every display currently in the snapshot.
    BlankAll,
    /// Force-wake every display currently in the snapshot.
    WakeAll,
    /// Force-blank a single display by id.
    BlankOne(String),
    /// Force-wake a single display by id.
    WakeOne(String),
    /// Open the daemon's web UI at `http://127.0.0.1:<port>`.
    OpenWebUi {
        /// The TCP port to open.
        port: u16,
    },
    /// Quit the tray.
    Quit,
}

/// Build the tray menu from the current snapshot and reachability.
///
/// The top-level layout is fixed; the per-display submenus are appended
/// from `snapshot.displays` so a reload that adds or removes a display
/// changes the menu shape on the next refresh.
///
/// Enabled-state rules:
///
/// - **Open web UI** + **Quit** are always clickable.
/// - **Pause / Resume** are clickable whenever the daemon is reachable —
///   they operate at the rule level, not per-display, so an empty
///   display list does not disable them.
/// - **Blank all / Wake all** need both reachability AND at least one
///   configured display.
/// - **Per-display submenus** stay openable (so the operator can see
///   what is inside) but every action inside is greyed out when the
///   daemon is unreachable.
#[must_use]
pub fn build_menu(
    snapshot: Option<&StateSnapshot>,
    unreachable: bool,
    web_port: u16,
) -> Vec<MenuEntry> {
    let displays: &[(_, _)] = match snapshot {
        Some(s) if !s.displays.is_empty() => &s.displays,
        _ => &[],
    };
    let can_pause = !unreachable;
    let can_blank_all = !unreachable && !displays.is_empty();

    let mut entries: Vec<MenuEntry> = Vec::with_capacity(14 + displays.len() * 4);

    // ── Pause / Resume ────────────────────────────────────────────────────
    entries.push(MenuEntry::Action {
        label: "Pause 30m".into(),
        enabled: can_pause,
        action: Action::Pause(Some(Duration::from_secs(30 * 60))),
    });
    entries.push(MenuEntry::Action {
        label: "Pause 2h".into(),
        enabled: can_pause,
        action: Action::Pause(Some(Duration::from_secs(2 * 60 * 60))),
    });
    entries.push(MenuEntry::Action {
        label: "Pause until resumed".into(),
        enabled: can_pause,
        action: Action::Pause(None),
    });
    entries.push(MenuEntry::Action {
        label: "Resume".into(),
        enabled: can_pause,
        action: Action::Resume,
    });

    // ── Separator ─────────────────────────────────────────────────────────
    entries.push(MenuEntry::Separator);

    // ── Blank all / Wake all ──────────────────────────────────────────────
    entries.push(MenuEntry::Action {
        label: "Blank all now".into(),
        enabled: can_blank_all,
        action: Action::BlankAll,
    });
    entries.push(MenuEntry::Action {
        label: "Wake all now".into(),
        enabled: can_blank_all,
        action: Action::WakeAll,
    });

    // ── Per-display submenus ──────────────────────────────────────────────
    if !displays.is_empty() {
        entries.push(MenuEntry::Separator);

        // Deterministic ordering: sort by display id so the menu is
        // stable across refreshes (the snapshot's Vec order is not
        // guaranteed and we want operator muscle-memory to work).
        let mut sorted: Vec<&(String, _)> = displays.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        for (id, d) in sorted {
            // Submenu shell stays openable regardless of reachability
            // so the operator can still inspect what's inside; the
            // children carry the disabled state.
            entries.push(MenuEntry::Submenu {
                label: format!("{id} — {}", d.phase),
                enabled: true,
                entries: vec![
                    MenuEntry::Action {
                        label: "Blank now".into(),
                        enabled: !unreachable,
                        action: Action::BlankOne(id.clone()),
                    },
                    MenuEntry::Action {
                        label: "Wake now".into(),
                        enabled: !unreachable,
                        action: Action::WakeOne(id.clone()),
                    },
                ],
            });
        }
    }

    // ── Open web UI ───────────────────────────────────────────────────────
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Action {
        label: "Open web UI".into(),
        enabled: true,
        action: Action::OpenWebUi { port: web_port },
    });

    // ── Quit ──────────────────────────────────────────────────────────────
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Action {
        label: "Quit".into(),
        enabled: true,
        action: Action::Quit,
    });

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::rules::{DisplaySnapshot, StateSnapshot};

    fn disp(id: &str, phase: &str) -> (String, DisplaySnapshot) {
        (
            id.into(),
            DisplaySnapshot {
                phase: phase.into(),
                inhibited: false,
                paused: false,
                cmd_gen: 0,
                controllers: vec![],
                stage: None,
            },
        )
    }

    fn snap(displays: Vec<(String, DisplaySnapshot)>) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays,
            pending_reload: None,
        }
    }

    fn labels(entries: &[MenuEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                MenuEntry::Action { label, .. } | MenuEntry::Submenu { label, .. } => label.clone(),
                MenuEntry::Separator => "──".into(),
            })
            .collect()
    }

    fn collect_actions(entries: &[MenuEntry]) -> Vec<Action> {
        let mut out = Vec::new();
        for e in entries {
            match e {
                MenuEntry::Action { action, .. } => out.push(action.clone()),
                MenuEntry::Submenu { entries, .. } => out.extend(collect_actions(entries)),
                MenuEntry::Separator => {}
            }
        }
        out
    }

    // ── Top-level layout ─────────────────────────────────────────────────

    #[test]
    fn top_level_has_pause_resume_blank_all_wake_all_open_quit() {
        let menu = build_menu(None, false, 8137);
        let actions = collect_actions(&menu);
        assert!(matches!(actions[0], Action::Pause(Some(d)) if d.as_secs() == 30 * 60));
        assert!(matches!(actions[1], Action::Pause(Some(d)) if d.as_secs() == 2 * 60 * 60));
        assert!(matches!(actions[2], Action::Pause(None)));
        assert!(matches!(actions[3], Action::Resume));
        assert!(matches!(actions[4], Action::BlankAll));
        assert!(matches!(actions[5], Action::WakeAll));
        assert!(matches!(actions[6], Action::OpenWebUi { port: 8137 }));
        assert!(matches!(actions[7], Action::Quit));
    }

    #[test]
    fn labels_match_dispatch_spec() {
        let menu = build_menu(None, false, 8137);
        let l = labels(&menu);
        assert_eq!(
            &l,
            &[
                "Pause 30m".to_string(),
                "Pause 2h".to_string(),
                "Pause until resumed".to_string(),
                "Resume".to_string(),
                "──".to_string(),
                "Blank all now".to_string(),
                "Wake all now".to_string(),
                "──".to_string(),
                "Open web UI".to_string(),
                "──".to_string(),
                "Quit".to_string(),
            ]
        );
    }

    // ── Per-display submenus ──────────────────────────────────────────────

    #[test]
    fn single_display_yields_one_submenu_with_per_display_actions() {
        let snap = snap(vec![disp("monitor", "active")]);
        let menu = build_menu(Some(&snap), false, 8137);

        // Find the submenu entry.
        let submenus: Vec<&MenuEntry> = menu
            .iter()
            .filter(|e| matches!(e, MenuEntry::Submenu { .. }))
            .collect();
        assert_eq!(submenus.len(), 1, "expected exactly one submenu");

        match &submenus[0] {
            MenuEntry::Submenu { label, entries, .. } => {
                assert_eq!(label, "monitor — active");
                assert_eq!(entries.len(), 2);
                match &entries[0] {
                    MenuEntry::Action { label, action, .. } => {
                        assert_eq!(label, "Blank now");
                        assert!(matches!(action, Action::BlankOne(id) if id == "monitor"));
                    }
                    other => panic!("expected Blank now action, got {other:?}"),
                }
                match &entries[1] {
                    MenuEntry::Action { label, action, .. } => {
                        assert_eq!(label, "Wake now");
                        assert!(matches!(action, Action::WakeOne(id) if id == "monitor"));
                    }
                    other => panic!("expected Wake now action, got {other:?}"),
                }
            }
            other => panic!("expected submenu, got {other:?}"),
        }
    }

    #[test]
    fn two_displays_yield_two_submenus_with_correct_targeting() {
        let snap = snap(vec![disp("monitor", "active"), disp("tv", "blanked")]);
        let menu = build_menu(Some(&snap), false, 8137);

        let submenus: Vec<&MenuEntry> = menu
            .iter()
            .filter(|e| matches!(e, MenuEntry::Submenu { .. }))
            .collect();
        assert_eq!(submenus.len(), 2);

        // Submenus are sorted by display id — monitor before tv.
        let (l0, e0) = match &submenus[0] {
            MenuEntry::Submenu { label, entries, .. } => (label.clone(), entries.clone()),
            _ => unreachable!(),
        };
        let (l1, e1) = match &submenus[1] {
            MenuEntry::Submenu { label, entries, .. } => (label.clone(), entries.clone()),
            _ => unreachable!(),
        };

        assert_eq!(l0, "monitor — active");
        assert_eq!(l1, "tv — blanked");

        // Verify each submenu targets its own display, not the other.
        let targets_0: Vec<&str> = e0
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Action {
                    action: Action::BlankOne(id),
                    ..
                }
                | MenuEntry::Action {
                    action: Action::WakeOne(id),
                    ..
                } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(targets_0, vec!["monitor", "monitor"]);

        let targets_1: Vec<&str> = e1
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Action {
                    action: Action::BlankOne(id),
                    ..
                }
                | MenuEntry::Action {
                    action: Action::WakeOne(id),
                    ..
                } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(targets_1, vec!["tv", "tv"]);
    }

    #[test]
    fn display_removed_after_reload_drops_submenu() {
        // Snapshot before reload: monitor + tv.
        let before = build_menu(
            Some(&snap(vec![
                disp("monitor", "active"),
                disp("tv", "blanked"),
            ])),
            false,
            8137,
        );
        let before_subs: Vec<&MenuEntry> = before
            .iter()
            .filter(|e| matches!(e, MenuEntry::Submenu { .. }))
            .collect();
        assert_eq!(before_subs.len(), 2);

        // Snapshot after reload: monitor only — tv removed in the new config.
        let after = build_menu(Some(&snap(vec![disp("monitor", "active")])), false, 8137);
        let after_subs: Vec<&MenuEntry> = after
            .iter()
            .filter(|e| matches!(e, MenuEntry::Submenu { .. }))
            .collect();
        assert_eq!(after_subs.len(), 1);
        match &after_subs[0] {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "monitor — active"),
            other => panic!("expected monitor submenu, got {other:?}"),
        }
    }

    #[test]
    fn display_added_after_reload_appears_as_new_submenu() {
        let before = build_menu(Some(&snap(vec![disp("monitor", "active")])), false, 8137);
        assert_eq!(
            before
                .iter()
                .filter(|e| matches!(e, MenuEntry::Submenu { .. }))
                .count(),
            1
        );

        let after = build_menu(
            Some(&snap(vec![
                disp("monitor", "active"),
                disp("kitchen", "staged"),
            ])),
            false,
            8137,
        );
        let sub_labels: Vec<String> = after
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Submenu { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            sub_labels,
            vec![
                "kitchen — staged".to_string(),
                "monitor — active".to_string()
            ]
        );
    }

    #[test]
    fn submenu_label_reflects_current_phase() {
        // A display that was active and is now blanked shows the new phase.
        let snap1 = snap(vec![disp("tv", "active")]);
        let menu1 = build_menu(Some(&snap1), false, 8137);
        match &menu1
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .unwrap()
        {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "tv — active"),
            _ => unreachable!(),
        }

        let snap2 = snap(vec![disp("tv", "blanked")]);
        let menu2 = build_menu(Some(&snap2), false, 8137);
        match &menu2
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .unwrap()
        {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "tv — blanked"),
            _ => unreachable!(),
        }
    }

    // ── Reachability / enabled state ─────────────────────────────────────

    #[test]
    fn unreachable_disables_all_mutations_but_keeps_open_web_ui_and_quit() {
        let snap = snap(vec![disp("monitor", "active")]);
        let menu = build_menu(Some(&snap), true, 8137);

        for entry in &menu {
            let actual_enabled = match entry {
                MenuEntry::Action { enabled, .. } => *enabled,
                MenuEntry::Submenu { entries, .. } => entries
                    .iter()
                    .all(|c| !matches!(c, MenuEntry::Action { enabled: true, .. })),
                MenuEntry::Separator => true,
            };
            // The dispatch spec says only Open web UI + Quit remain
            // clickable when the daemon is unreachable; everything else
            // (Pause / Resume / Blank all / Wake all / submenu actions)
            // is greyed out.
            let expected_enabled = match entry {
                MenuEntry::Action { action, .. } => {
                    matches!(action, Action::OpenWebUi { .. } | Action::Quit)
                }
                MenuEntry::Submenu { .. } | MenuEntry::Separator => true,
            };
            assert_eq!(
                actual_enabled, expected_enabled,
                "entry {entry:?} enabled-state mismatch when unreachable"
            );
        }
    }

    #[test]
    fn empty_displays_disables_blank_and_wake_but_not_pause() {
        let snap = snap(vec![]);
        let menu = build_menu(Some(&snap), false, 8137);
        // Pause items: enabled (operate at rule level; rule=None still
        // applies even when the display list is empty).
        assert!(matches!(menu[0], MenuEntry::Action { enabled: true, .. }));
        assert!(matches!(menu[3], MenuEntry::Action { enabled: true, .. }));
        // Blank all now / Wake all now: disabled (nothing to target).
        assert!(matches!(menu[5], MenuEntry::Action { enabled: false, .. }));
        assert!(matches!(menu[6], MenuEntry::Action { enabled: false, .. }));
        // Quit / Open web UI: enabled.
        assert!(matches!(menu[10], MenuEntry::Action { enabled: true, .. }));
        assert!(matches!(menu[8], MenuEntry::Action { enabled: true, .. }));
    }

    #[test]
    fn quit_and_open_always_enabled_even_without_snapshot_and_unreachable() {
        let menu = build_menu(None, true, 8137);
        // Pause/Resume: disabled.
        assert!(matches!(menu[0], MenuEntry::Action { enabled: false, .. }));
        // Blank all / Wake all: disabled.
        assert!(matches!(menu[5], MenuEntry::Action { enabled: false, .. }));
        assert!(matches!(menu[6], MenuEntry::Action { enabled: false, .. }));
        // Open web UI + Quit: enabled.
        assert!(matches!(menu[8], MenuEntry::Action { enabled: true, .. }));
        assert!(matches!(menu[10], MenuEntry::Action { enabled: true, .. }));
    }

    #[test]
    fn web_port_threads_through_open_web_ui() {
        let menu = build_menu(None, true, 4242);
        match &menu[8] {
            MenuEntry::Action { action, .. } => match action {
                Action::OpenWebUi { port } => assert_eq!(*port, 4242),
                other => panic!("expected OpenWebUi, got {other:?}"),
            },
            other => panic!("expected Action entry, got {other:?}"),
        }
    }

    #[test]
    fn submenu_entries_disabled_when_unreachable() {
        let snap = snap(vec![disp("monitor", "active")]);
        let menu = build_menu(Some(&snap), true, 8137);
        let sub = menu
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .expect("submenu present");
        match sub {
            MenuEntry::Submenu {
                entries, enabled, ..
            } => {
                // The submenu itself remains openable; its children are disabled.
                assert!(*enabled);
                for e in entries {
                    assert!(
                        matches!(e, MenuEntry::Action { enabled: false, .. }),
                        "submenu child should be disabled when unreachable: {e:?}"
                    );
                }
            }
            _ => unreachable!(),
        }
    }
}
