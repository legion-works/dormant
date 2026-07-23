//! Pure-logic menu model construction.
//!
//! The tray's menu is a fixed top-level layout (pause/resume + global
//! blank/wake + per-display submenus + web UI + quit) with **per-display
//! submenus** driven by the live [`StateSnapshot`].  Each display becomes
//! a submenu labeled `"<glyph> <display-id> — <phase>"` (e.g. `"● monitor
//! — active"`, `"◐ tv — staged: render_black"`, `"○ main — blanked"`)
//! containing `Blank now` / `Wake now` entries targeting that specific
//! display via [`Action::BlankOne`] / [`Action::WakeOne`].
//!
//! Every clickable item carries a [`Glyph`] (rendered to PNG at build
//! time and exposed via `ksni::StandardItem.icon_data`).  Tier-1 polish:
//! the menu chrome stays Plasma-native, dormant's identity travels
//! inside the menu structure as the per-item icon.
//!
//! The menu rebuilds whenever the IPC loop applies a new snapshot — the
//! tray's `menu()` callback hands `ksni` the freshly-built `Vec<MenuEntry>`
//! on every refresh, so displays that disappear across a daemon reload
//! vanish from the menu without any explicit cleanup.
//!
//! ## Phase glyph legend (`DBusMenu` labels are plain text — no styling)
//!
//! - `●` filled circle: display is in `active` (no blanking in progress).
//! - `◐` half-filled circle: display is in `staged` (escalating the
//!   blanking ladder; the submenu label also includes the stage kind,
//!   e.g. `"staged: render_black"`).
//! - `○` empty circle: display is fully `blanked`.  Also used for the
//!   transitional phases `grace`, `blanking`, `waking` — those are
//!   short-lived and the explicit phase name in the label is enough
//!   to disambiguate.
//! - `⚠` warning triangle: reserved for per-display unreachability
//!   (not currently surfaced — the snapshot doesn't carry it; the
//!   top-level `unreachable` glyph is the mark's greyed variant).

use std::time::Duration;

use dormant_core::{config::DisplayScope, rules::StateSnapshot, traits::PowerState};

use crate::icon::Glyph;

/// One top-level menu entry.  Submenus recurse via [`MenuEntry::Submenu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// A clickable leaf item with a glyph and an action.
    Action {
        /// The visible label (plain text — `DBusMenu` doesn't carry inline styling).
        label: String,
        /// Whether the entry is currently clickable.
        enabled: bool,
        /// The glyph shown next to the label; PNG bytes come from [`Glyph::png_bytes`].
        icon: Glyph,
        /// The action to dispatch on click.
        action: Action,
    },
    /// A visual separator (no click target).
    Separator,
    /// A submenu containing more entries.
    Submenu {
        /// The visible label (e.g. `"● monitor — active"`).
        label: String,
        /// Whether the submenu's child actions are enabled.  The submenu
        /// itself is always openable so the operator can still see why
        /// blank/wake are greyed out.
        enabled: bool,
        /// The entries inside the submenu (typically `Blank now` /
        /// `Wake now` for one display).
        entries: Vec<MenuEntry>,
    },
    /// A disabled, non-interactive status line — e.g. `"Paused — Resume
    /// to restore"` while the daemon is paused.
    Info {
        /// The visible label.
        label: String,
        /// The glyph shown next to the label.
        icon: Glyph,
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

/// Glyph that decorates a given action.  Centralised so the menu
/// builder doesn't repeat the same `match` everywhere — and so adding
/// a new [`Action`] variant surfaces a compile error here instead of
/// silently going icon-less.
fn glyph_for(action: &Action) -> Glyph {
    match action {
        Action::Pause(_) | Action::Separator => Glyph::Pause,
        Action::Resume => Glyph::Play,
        Action::BlankAll | Action::BlankOne(_) => Glyph::DisplayOff,
        Action::WakeAll | Action::WakeOne(_) => Glyph::DisplayOn,
        Action::OpenWebUi { .. } => Glyph::Web,
        Action::Quit => Glyph::Exit,
    }
}

/// The phase-glyph prefix for a per-display submenu label.
///
/// See the module-level legend for the chosen mapping; the function
/// keeps the formatting consistent (single space between glyph and
/// display id).
fn submenu_label(display_id: &str, d: &dormant_core::rules::DisplaySnapshot) -> String {
    let glyph = match d.phase.as_str() {
        "active" => "●",
        "staged" => "◐",
        _ => "○",
    };
    // Staged displays carry the active stage kind in the label so the
    // operator can tell `staged: render_black` from `staged: screensaver`
    // without opening the submenu.
    let phase_text = match (&d.stage, d.phase.as_str()) {
        (Some(stage), "staged") => format!("staged: {}", stage_kind_label(stage.kind)),
        (_, phase) => phase.to_string(),
    };
    if d.scope == DisplayScope::Shared {
        let ownership = if d.owned { "owner" } else { "deferred" };
        let panel = match d.panel_state.as_ref().and_then(|state| state.power) {
            Some(PowerState::On) => "ON",
            Some(PowerState::Standby) => "OFF",
            None => "unknown",
        };
        format!("{glyph} {display_id} — {phase_text} — {ownership} — panel {panel}")
    } else {
        format!("{glyph} {display_id} — {phase_text}")
    }
}

/// Stringify a [`StageKind`] for display.  Uses the serde
/// `snake_case` representation so it matches the literal name in the
/// daemon config.
fn stage_kind_label(kind: dormant_core::types::StageKind) -> &'static str {
    use dormant_core::types::{BlankMode, StageKind};
    match kind {
        StageKind::Controller(BlankMode::PowerOff) => "controller: power_off",
        StageKind::Controller(BlankMode::ScreenOffAudioOn) => "controller: screen_off_audio_on",
        StageKind::Controller(BlankMode::BrightnessZero) => "controller: brightness_zero",
        StageKind::RenderBlack => "render_black",
        StageKind::RenderScreensaver => "render_screensaver",
    }
}

/// True if any display in the snapshot reports `paused: true`.  The
/// snapshot does not carry pause duration — that's a daemon-internal
/// detail — so we can only show "something is paused", not "which
/// pause option triggered it".
fn any_paused(snapshot: Option<&StateSnapshot>) -> bool {
    snapshot.is_some_and(|s| s.displays.iter().any(|(_, d)| d.paused))
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
/// - When **any display is paused**, a disabled `Info` line `"Paused —
///   Resume to restore"` is inserted above the Pause items, and Resume
///   becomes the only enabled pause-row item.
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
    let paused = any_paused(snapshot) && !unreachable;

    let mut entries: Vec<MenuEntry> = Vec::with_capacity(16 + displays.len() * 4);

    // ── Paused-state info line (only when something IS paused) ─────────
    if paused {
        entries.push(MenuEntry::Info {
            label: "Paused — Resume to restore".to_string(),
            icon: Glyph::Info,
        });
    }

    // ── Pause / Resume ────────────────────────────────────────────────────
    entries.push(MenuEntry::Action {
        label: "Pause 30m".into(),
        enabled: can_pause && !paused,
        icon: glyph_for(&Action::Pause(Some(Duration::from_secs(30 * 60)))),
        action: Action::Pause(Some(Duration::from_secs(30 * 60))),
    });
    entries.push(MenuEntry::Action {
        label: "Pause 2h".into(),
        enabled: can_pause && !paused,
        icon: glyph_for(&Action::Pause(Some(Duration::from_secs(2 * 60 * 60)))),
        action: Action::Pause(Some(Duration::from_secs(2 * 60 * 60))),
    });
    entries.push(MenuEntry::Action {
        label: "Pause until resumed".into(),
        enabled: can_pause && !paused,
        icon: glyph_for(&Action::Pause(None)),
        action: Action::Pause(None),
    });
    entries.push(MenuEntry::Action {
        label: "Resume".into(),
        enabled: paused, // only meaningful when something is paused
        icon: glyph_for(&Action::Resume),
        action: Action::Resume,
    });

    // ── Separator ─────────────────────────────────────────────────────────
    entries.push(MenuEntry::Separator);

    // ── Blank all / Wake all ──────────────────────────────────────────────
    entries.push(MenuEntry::Action {
        label: "Blank all now".into(),
        enabled: can_blank_all,
        icon: glyph_for(&Action::BlankAll),
        action: Action::BlankAll,
    });
    entries.push(MenuEntry::Action {
        label: "Wake all now".into(),
        enabled: can_blank_all,
        icon: glyph_for(&Action::WakeAll),
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
            let label = submenu_label(id, d);
            let blank_label = if d.scope == DisplayScope::Shared {
                "Blank shared panel — affects all connected machines"
            } else {
                "Blank now"
            };
            // Submenu shell stays openable regardless of reachability
            // so the operator can still inspect what's inside; the
            // children carry the disabled state.
            entries.push(MenuEntry::Submenu {
                label,
                enabled: true,
                entries: vec![
                    MenuEntry::Action {
                        label: blank_label.into(),
                        enabled: !unreachable,
                        icon: glyph_for(&Action::BlankOne(id.clone())),
                        action: Action::BlankOne(id.clone()),
                    },
                    MenuEntry::Action {
                        label: "Wake now".into(),
                        enabled: !unreachable,
                        icon: glyph_for(&Action::WakeOne(id.clone())),
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
        icon: glyph_for(&Action::OpenWebUi { port: web_port }),
        action: Action::OpenWebUi { port: web_port },
    });

    // ── Quit ──────────────────────────────────────────────────────────────
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Action {
        label: "Quit".into(),
        enabled: true,
        icon: glyph_for(&Action::Quit),
        action: Action::Quit,
    });

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::types::StageKind;
    use dormant_core::{
        config::DisplayScope,
        rules::{DisplaySnapshot, StageInfo, StateSnapshot},
        traits::{PanelState, PowerState},
    };

    fn disp(id: &str, phase: &str) -> (String, DisplaySnapshot) {
        disp_with(id, phase, false, None)
    }

    fn disp_with(
        id: &str,
        phase: &str,
        paused: bool,
        stage: Option<StageInfo>,
    ) -> (String, DisplaySnapshot) {
        (
            id.into(),
            DisplaySnapshot {
                phase: phase.into(),
                inhibited: false,
                paused,
                cmd_gen: 0,
                scope: dormant_core::config::DisplayScope::Private,
                owned: true,
                observed_input_code: None,
                panel_state: None,
                controllers: vec![],
                wake_attempts: 0,
                last_blank_failed: false,
                stage,
            },
        )
    }

    fn shared_disp(
        id: &str,
        phase: &str,
        owned: bool,
        power: Option<PowerState>,
    ) -> (String, DisplaySnapshot) {
        let (id, mut display) = disp(id, phase);
        display.scope = DisplayScope::Shared;
        display.owned = owned;
        display.panel_state = power.map(|power| PanelState {
            power: Some(power),
            brightness: None,
        });
        (id, display)
    }

    fn snap(displays: Vec<(String, DisplaySnapshot)>) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays,
            pending_reload: None,
            rollback: None,
        }
    }

    fn labels(entries: &[MenuEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                MenuEntry::Action { label, .. }
                | MenuEntry::Submenu { label, .. }
                | MenuEntry::Info { label, .. } => label.clone(),
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
                MenuEntry::Separator | MenuEntry::Info { .. } => {}
            }
        }
        out
    }

    /// Locate the action entry with the given label substring (linear scan).
    fn find_action<'a>(entries: &'a [MenuEntry], needle: &str) -> Option<&'a MenuEntry> {
        entries.iter().find(|e| match e {
            MenuEntry::Action { label, .. } => label.contains(needle),
            _ => false,
        })
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

    // ── Per-item glyphs ──────────────────────────────────────────────────

    #[test]
    fn every_action_carries_a_glyph() {
        let menu = build_menu(None, false, 8137);
        for entry in &menu {
            match entry {
                MenuEntry::Action { icon, .. } => {
                    // `png_bytes` must always return non-empty data
                    // (the include_bytes! blobs are built unconditionally).
                    assert!(!icon.png_bytes().is_empty(), "glyph has no PNG bytes");
                }
                MenuEntry::Separator | MenuEntry::Info { .. } | MenuEntry::Submenu { .. } => {}
            }
        }
    }

    #[test]
    fn pause_actions_use_pause_glyph() {
        let menu = build_menu(None, false, 8137);
        for label in ["Pause 30m", "Pause 2h", "Pause until resumed"] {
            let entry = find_action(&menu, label).expect(label);
            match entry {
                MenuEntry::Action { icon, action, .. } => {
                    assert_eq!(*icon, Glyph::Pause, "{label}: wrong glyph");
                    assert!(matches!(action, Action::Pause(_)));
                }
                _ => panic!("{label}: not an Action"),
            }
        }
    }

    #[test]
    fn resume_uses_play_glyph() {
        let menu = build_menu(
            Some(&snap(vec![disp("monitor", "blanked")])), // paused
            false,
            8137,
        );
        let entry = find_action(&menu, "Resume").expect("Resume entry");
        match entry {
            MenuEntry::Action { icon, action, .. } => {
                assert_eq!(*icon, Glyph::Play);
                assert!(matches!(action, Action::Resume));
            }
            _ => panic!("Resume: not an Action"),
        }
    }

    #[test]
    fn blank_actions_use_display_off_glyph() {
        let snap = snap(vec![disp("monitor", "active")]);
        let menu = build_menu(Some(&snap), false, 8137);
        // Top-level Blank all now.
        let entry = find_action(&menu, "Blank all now").expect("Blank all now");
        match entry {
            MenuEntry::Action { icon, action, .. } => {
                assert_eq!(*icon, Glyph::DisplayOff);
                assert!(matches!(action, Action::BlankAll));
            }
            _ => panic!("Blank all now: not an Action"),
        }
        // Per-display Blank now inside the submenu.
        for e in &menu {
            if let MenuEntry::Submenu { entries, .. } = e {
                if let MenuEntry::Action { icon, action, .. } = &entries[0] {
                    assert_eq!(*icon, Glyph::DisplayOff, "Blank now glyph");
                    assert!(matches!(action, Action::BlankOne(id) if id == "monitor"));
                }
                if let MenuEntry::Action { icon, action, .. } = &entries[1] {
                    assert_eq!(*icon, Glyph::DisplayOn, "Wake now glyph");
                    assert!(matches!(action, Action::WakeOne(id) if id == "monitor"));
                }
            }
        }
    }

    #[test]
    fn open_web_ui_and_quit_carry_their_own_glyphs() {
        let menu = build_menu(None, false, 8137);
        let open = find_action(&menu, "Open web UI").unwrap();
        match open {
            MenuEntry::Action { icon, .. } => assert_eq!(*icon, Glyph::Web),
            _ => panic!("Open web UI: not an Action"),
        }
        let quit = find_action(&menu, "Quit").unwrap();
        match quit {
            MenuEntry::Action { icon, .. } => assert_eq!(*icon, Glyph::Exit),
            _ => panic!("Quit: not an Action"),
        }
    }

    // ── Pause-state feedback ─────────────────────────────────────────────

    #[test]
    fn paused_snapshot_inserts_info_line_and_enables_resume() {
        // At least one display reports paused=true.
        let snap = snap(vec![disp_with("monitor", "blanked", true, None)]);
        let menu = build_menu(Some(&snap), false, 8137);

        // The Info line is first, with the info glyph and the
        // prescribed label.
        match &menu[0] {
            MenuEntry::Info { label, icon } => {
                assert_eq!(label, "Paused — Resume to restore");
                assert_eq!(*icon, Glyph::Info);
            }
            other => panic!("expected Info entry first when paused: {other:?}"),
        }

        // Resume is enabled (and only Resume among the pause items).
        let resume = find_action(&menu, "Resume").expect("Resume");
        match resume {
            MenuEntry::Action { enabled, .. } => {
                assert!(*enabled, "Resume should be enabled when paused");
            }
            _ => panic!("Resume: not an Action"),
        }
        for label in ["Pause 30m", "Pause 2h", "Pause until resumed"] {
            let entry = find_action(&menu, label).expect(label);
            match entry {
                MenuEntry::Action { enabled, .. } => {
                    assert!(!*enabled, "{label} should be disabled when paused");
                }
                _ => panic!("{label}: not an Action"),
            }
        }
    }

    #[test]
    fn unpaused_snapshot_omits_info_line_and_disables_resume() {
        let snap = snap(vec![disp("monitor", "active")]);
        let menu = build_menu(Some(&snap), false, 8137);

        // No Info entry in the unpaused path.
        assert!(
            !menu.iter().any(|e| matches!(e, MenuEntry::Info { .. })),
            "Info line should not appear when nothing is paused: {menu:?}"
        );

        // Resume is disabled when nothing is paused — clicking it would
        // be a confusing no-op.
        let resume = find_action(&menu, "Resume").expect("Resume");
        match resume {
            MenuEntry::Action { enabled, .. } => {
                assert!(!*enabled, "Resume should be disabled when not paused");
            }
            _ => panic!("Resume: not an Action"),
        }
    }

    #[test]
    fn unreachable_daemon_omits_info_even_when_displays_were_paused() {
        // The paused-state flag carries over from a previous snapshot,
        // but reachability is the upstream gate — if the daemon is
        // gone we don't know whether it is still paused.
        let snap = snap(vec![disp_with("monitor", "blanked", true, None)]);
        let menu = build_menu(Some(&snap), true, 8137);
        assert!(
            !menu.iter().any(|e| matches!(e, MenuEntry::Info { .. })),
            "Info line should not appear when daemon is unreachable"
        );
    }

    // ── Phase glyphs in submenu labels ──────────────────────────────────

    #[test]
    fn submenu_label_uses_phase_glyph() {
        // active → ●
        let menu = build_menu(Some(&snap(vec![disp("tv", "active")])), false, 8137);
        let sub = menu
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .unwrap();
        match sub {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "● tv — active"),
            _ => unreachable!(),
        }

        // blanked → ○
        let menu = build_menu(Some(&snap(vec![disp("tv", "blanked")])), false, 8137);
        let sub = menu
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .unwrap();
        match sub {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "○ tv — blanked"),
            _ => unreachable!(),
        }

        // staged with a known stage kind → ◐ <id> — staged: <kind>
        let menu = build_menu(
            Some(&snap(vec![disp_with(
                "tv",
                "staged",
                false,
                Some(StageInfo {
                    idx: 1,
                    kind: StageKind::RenderBlack,
                }),
            )])),
            false,
            8137,
        );
        let sub = menu
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .unwrap();
        match sub {
            MenuEntry::Submenu { label, .. } => {
                assert_eq!(label, "◐ tv — staged: render_black");
            }
            _ => unreachable!(),
        }

        // transitional phases use ○ plus the explicit phase name
        for phase in ["grace", "blanking", "waking"] {
            let menu = build_menu(Some(&snap(vec![disp("tv", phase)])), false, 8137);
            let sub = menu
                .iter()
                .find(|e| matches!(e, MenuEntry::Submenu { .. }))
                .unwrap();
            match sub {
                MenuEntry::Submenu { label, .. } => {
                    assert_eq!(
                        label.as_str(),
                        format!("○ tv — {phase}").as_str(),
                        "phase {phase} label"
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    // ── Per-display submenus ──────────────────────────────────────────────

    #[test]
    fn single_display_yields_one_submenu_with_per_display_actions() {
        let snap = snap(vec![disp("monitor", "active")]);
        let menu = build_menu(Some(&snap), false, 8137);

        let submenus: Vec<&MenuEntry> = menu
            .iter()
            .filter(|e| matches!(e, MenuEntry::Submenu { .. }))
            .collect();
        assert_eq!(submenus.len(), 1, "expected exactly one submenu");

        match &submenus[0] {
            MenuEntry::Submenu { label, entries, .. } => {
                assert_eq!(label, "● monitor — active");
                assert_eq!(entries.len(), 2);
                match &entries[0] {
                    MenuEntry::Action {
                        label,
                        action,
                        icon,
                        ..
                    } => {
                        assert_eq!(label, "Blank now");
                        assert_eq!(*icon, Glyph::DisplayOff);
                        assert!(matches!(action, Action::BlankOne(id) if id == "monitor"));
                    }
                    other => panic!("expected Blank now action, got {other:?}"),
                }
                match &entries[1] {
                    MenuEntry::Action {
                        label,
                        action,
                        icon,
                        ..
                    } => {
                        assert_eq!(label, "Wake now");
                        assert_eq!(*icon, Glyph::DisplayOn);
                        assert!(matches!(action, Action::WakeOne(id) if id == "monitor"));
                    }
                    other => panic!("expected Wake now action, got {other:?}"),
                }
            }
            other => panic!("expected submenu, got {other:?}"),
        }
    }

    #[test]
    fn shared_submenu_shows_owner_and_panel_state() {
        let snapshot = snap(vec![shared_disp(
            "tv",
            "blanked",
            true,
            Some(PowerState::On),
        )]);
        let menu = build_menu(Some(&snapshot), false, 8137);

        let submenu = menu
            .iter()
            .find(|entry| matches!(entry, MenuEntry::Submenu { .. }))
            .expect("shared display submenu");
        assert!(matches!(submenu, MenuEntry::Submenu { label, .. }
            if label == "○ tv — blanked — owner — panel ON"));
    }

    #[test]
    fn shared_nonowner_off_still_offers_wake() {
        let snapshot = snap(vec![shared_disp(
            "tv",
            "active",
            false,
            Some(PowerState::Standby),
        )]);
        let menu = build_menu(Some(&snapshot), false, 8137);

        let submenu = menu
            .iter()
            .find(|entry| matches!(entry, MenuEntry::Submenu { .. }))
            .expect("shared display submenu");
        let MenuEntry::Submenu { label, entries, .. } = submenu else {
            unreachable!("expected shared display submenu");
        };
        assert_eq!(label, "● tv — active — deferred — panel OFF");
        assert!(matches!(
            entries.get(1),
            Some(MenuEntry::Action {
                label,
                enabled: true,
                action: Action::WakeOne(id),
                ..
            }) if label == "Wake now" && id == "tv"
        ));
    }

    #[test]
    fn shared_blank_warns_all_connected_machines() {
        let snapshot = snap(vec![shared_disp("tv", "active", true, None)]);
        let menu = build_menu(Some(&snapshot), false, 8137);

        let submenu = menu
            .iter()
            .find(|entry| matches!(entry, MenuEntry::Submenu { .. }))
            .expect("shared display submenu");
        let MenuEntry::Submenu { entries, .. } = submenu else {
            unreachable!("expected shared display submenu");
        };
        assert!(matches!(
            entries.first(),
            Some(MenuEntry::Action { label, .. })
                if label == "Blank shared panel — affects all connected machines"
        ));
    }

    #[test]
    fn private_display_labels_remain_byte_identical() {
        let snapshot = snap(vec![disp("private-panel", "blanked")]);
        let menu = build_menu(Some(&snapshot), false, 8137);

        let submenu = menu
            .iter()
            .find(|entry| matches!(entry, MenuEntry::Submenu { .. }))
            .expect("private display submenu");
        let MenuEntry::Submenu { label, entries, .. } = submenu else {
            unreachable!("expected private display submenu");
        };
        assert_eq!(label, "○ private-panel — blanked");
        assert!(matches!(entries.as_slice(), [
            MenuEntry::Action { label: blank_label, .. },
            MenuEntry::Action { label: wake_label, .. },
        ] if blank_label == "Blank now" && wake_label == "Wake now"));
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

        let (l0, e0) = match &submenus[0] {
            MenuEntry::Submenu { label, entries, .. } => (label.clone(), entries.clone()),
            _ => unreachable!(),
        };
        let (l1, e1) = match &submenus[1] {
            MenuEntry::Submenu { label, entries, .. } => (label.clone(), entries.clone()),
            _ => unreachable!(),
        };

        assert_eq!(l0, "● monitor — active");
        assert_eq!(l1, "○ tv — blanked");

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

        let after = build_menu(Some(&snap(vec![disp("monitor", "active")])), false, 8137);
        let after_subs: Vec<&MenuEntry> = after
            .iter()
            .filter(|e| matches!(e, MenuEntry::Submenu { .. }))
            .collect();
        assert_eq!(after_subs.len(), 1);
        match &after_subs[0] {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "● monitor — active"),
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
                "◐ kitchen — staged".to_string(),
                "● monitor — active".to_string(),
            ]
        );
    }

    #[test]
    fn submenu_label_reflects_current_phase() {
        let snap1 = snap(vec![disp("tv", "active")]);
        let menu1 = build_menu(Some(&snap1), false, 8137);
        match &menu1
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .unwrap()
        {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "● tv — active"),
            _ => unreachable!(),
        }

        let snap2 = snap(vec![disp("tv", "blanked")]);
        let menu2 = build_menu(Some(&snap2), false, 8137);
        match &menu2
            .iter()
            .find(|e| matches!(e, MenuEntry::Submenu { .. }))
            .unwrap()
        {
            MenuEntry::Submenu { label, .. } => assert_eq!(label, "○ tv — blanked"),
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
                MenuEntry::Info { .. } | MenuEntry::Separator => true,
            };
            let expected_enabled = match entry {
                MenuEntry::Action { action, .. } => {
                    matches!(action, Action::OpenWebUi { .. } | Action::Quit)
                }
                MenuEntry::Submenu { .. } | MenuEntry::Info { .. } | MenuEntry::Separator => true,
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
        assert!(matches!(menu[0], MenuEntry::Action { enabled: true, .. }));
        assert!(matches!(menu[3], MenuEntry::Action { enabled: false, .. })); // Resume (nothing paused)
        assert!(matches!(menu[5], MenuEntry::Action { enabled: false, .. }));
        assert!(matches!(menu[6], MenuEntry::Action { enabled: false, .. }));
        assert!(matches!(menu[10], MenuEntry::Action { enabled: true, .. }));
        assert!(matches!(menu[8], MenuEntry::Action { enabled: true, .. }));
    }

    #[test]
    fn quit_and_open_always_enabled_even_without_snapshot_and_unreachable() {
        let menu = build_menu(None, true, 8137);
        assert!(matches!(menu[0], MenuEntry::Action { enabled: false, .. }));
        assert!(matches!(menu[5], MenuEntry::Action { enabled: false, .. }));
        assert!(matches!(menu[6], MenuEntry::Action { enabled: false, .. }));
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
