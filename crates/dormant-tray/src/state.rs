//! Pure-logic icon-state derivation from a [`StateSnapshot`].
//!
//! The tray's icon carries five semantic states:
//!
//! - [`IconState::Normal`] — every display in the engine is `active` and
//!   no display is paused.
//! - [`IconState::Attention`] — at least one display is in a non-active
//!   phase (`grace`, `blanking`, `staged`, `blanked`, `waking`) — i.e. the
//!   engine is currently working on something the operator might want to
//!   see.  The mark stays the brand green; the tooltip exposes the
//!   per-display detail.
//! - [`IconState::Paused`] — any display reports `paused: true` (an
//!   operator pause is in effect).  Paused overrides Attention because a
//!   human deliberately disabled blanking; the overlay badge communicates
//!   that intent.
//! - [`IconState::Failure`] — any display has accumulated wake attempts
//!   (`wake_attempts > 0`) or failed its last blank (`last_blank_failed`).
//!   Failure outranks Paused: a failing display needs the operator's
//!   attention even if the display happens to also be paused.
//! - [`IconState::Unreachable`] — the IPC socket could not be reached
//!   when the snapshot was taken.  This is set by the caller (the IPC
//!   loop); once set it sticks until a fresh snapshot arrives, even if a
//!   stale snapshot says "Normal" — fail-safe presence principle.

use dormant_core::rules::StateSnapshot;

/// The five tray icon states.  Cheap `Copy` — the enum has no payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// All displays active, no pauses, daemon reachable.
    Normal,
    /// At least one display in a non-active phase.
    Attention,
    /// At least one display paused (operator override).
    Paused,
    /// At least one display is failing to wake or failed its last blank
    /// attempt (`wake_attempts > 0 || last_blank_failed`).
    Failure,
    /// Daemon unreachable (IPC socket down).
    Unreachable,
}

/// Derive the icon state from a fresh [`StateSnapshot`].
///
/// This is the *snapshot-only* derivation: it does NOT account for
/// connectivity.  The IPC loop layers `Unreachable` on top of this
/// result when its reconnect timer is armed.
///
/// Precedence (highest first): **Failure** > **Paused** > **Attention** >
/// **Normal**.  (`Unreachable` is layered on top by the caller — the IPC
/// loop — and is not derived here.)
///
/// # Examples
///
/// ```
/// use dormant_tray::state::{derive_icon_state, IconState};
/// use dormant_core::rules::{DisplaySnapshot, StateSnapshot};
///
/// let snap = StateSnapshot {
///     sensors: vec![],
///     zones: vec![],
///     displays: vec![(
///         "mon".into(),
///         DisplaySnapshot {
///             phase: "blanked".into(),
///             inhibited: false,
///             paused: false,
///             cmd_gen: 0,
///             controllers: vec![],
///             wake_attempts: 0,
///             last_blank_failed: false,
///             stage: None,
///         },
///     )],
///     pending_reload: None,
///     rollback: None,
/// };
/// assert_eq!(derive_icon_state(&snap), IconState::Attention);
/// ```
#[must_use]
pub fn derive_icon_state(snap: &StateSnapshot) -> IconState {
    if snap.displays.is_empty() {
        // Empty snapshot — no displays to worry about.  Treat as Normal:
        // the operator has nothing configured yet, the icon stays in its
        // calm brand state.
        return IconState::Normal;
    }

    let any_failing = snap
        .displays
        .iter()
        .any(|(_id, d)| d.wake_attempts > 0 || d.last_blank_failed);
    if any_failing {
        return IconState::Failure;
    }

    let any_paused = snap.displays.iter().any(|(_id, d)| d.paused);
    if any_paused {
        return IconState::Paused;
    }

    let any_non_active = snap.displays.iter().any(|(_id, d)| d.phase != "active");
    if any_non_active {
        return IconState::Attention;
    }

    IconState::Normal
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::rules::{DisplaySnapshot, StateSnapshot};

    fn snap_with(phase: &str, paused: bool) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
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
                    stage: None,
                },
            )],
            pending_reload: None,
            rollback: None,
        }
    }

    fn snap_failing(phase: &str, wake_attempts: u64, last_blank_failed: bool) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
                DisplaySnapshot {
                    phase: phase.into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts,
                    last_blank_failed,
                    stage: None,
                },
            )],
            pending_reload: None,
            rollback: None,
        }
    }

    fn snap_failing_paused(
        phase: &str,
        wake_attempts: u64,
        last_blank_failed: bool,
        paused: bool,
    ) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "mon".into(),
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
                    wake_attempts,
                    last_blank_failed,
                    stage: None,
                },
            )],
            pending_reload: None,
            rollback: None,
        }
    }

    fn snap_with_two(a: (&str, bool), b: (&str, bool)) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![
                (
                    "a".into(),
                    DisplaySnapshot {
                        phase: a.0.into(),
                        inhibited: false,
                        paused: a.1,
                        cmd_gen: 0,
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
                    "b".into(),
                    DisplaySnapshot {
                        phase: b.0.into(),
                        inhibited: false,
                        paused: b.1,
                        cmd_gen: 0,
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
        }
    }

    #[test]
    fn empty_snapshot_is_normal() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
            rollback: None,
        };
        assert_eq!(derive_icon_state(&snap), IconState::Normal);
    }

    #[test]
    fn single_active_display_is_normal() {
        assert_eq!(
            derive_icon_state(&snap_with("active", false)),
            IconState::Normal
        );
    }

    #[test]
    fn single_blanked_display_is_attention() {
        assert_eq!(
            derive_icon_state(&snap_with("blanked", false)),
            IconState::Attention
        );
    }

    #[test]
    fn single_grace_display_is_attention() {
        assert_eq!(
            derive_icon_state(&snap_with("grace", false)),
            IconState::Attention
        );
    }

    #[test]
    fn single_staged_display_is_attention() {
        assert_eq!(
            derive_icon_state(&snap_with("staged", false)),
            IconState::Attention
        );
    }

    #[test]
    fn paused_display_overrides_attention() {
        // Phase=blanked + paused=true → Paused (not Attention).
        assert_eq!(
            derive_icon_state(&snap_with("blanked", true)),
            IconState::Paused
        );
    }

    #[test]
    fn paused_active_display_is_paused() {
        assert_eq!(
            derive_icon_state(&snap_with("active", true)),
            IconState::Paused
        );
    }

    #[test]
    fn mixed_two_displays_active_and_blanked_is_attention() {
        assert_eq!(
            derive_icon_state(&snap_with_two(("active", false), ("blanked", false))),
            IconState::Attention
        );
    }

    #[test]
    fn mixed_two_displays_blanked_one_paused_is_paused() {
        assert_eq!(
            derive_icon_state(&snap_with_two(("blanked", false), ("active", true))),
            IconState::Paused
        );
    }

    #[test]
    fn two_active_displays_is_normal() {
        assert_eq!(
            derive_icon_state(&snap_with_two(("active", false), ("active", false))),
            IconState::Normal
        );
    }

    #[test]
    fn wake_failing_display_is_failure() {
        assert_eq!(
            derive_icon_state(&snap_failing("blanked", 3, false)),
            IconState::Failure
        );
    }

    #[test]
    fn blank_failed_active_display_is_failure() {
        assert_eq!(
            derive_icon_state(&snap_failing("active", 0, true)),
            IconState::Failure
        );
    }

    #[test]
    fn failure_outranks_paused() {
        assert_eq!(
            derive_icon_state(&snap_failing_paused("active", 3, false, true)),
            IconState::Failure
        );
    }

    #[test]
    fn healthy_displays_unchanged() {
        // regression
        assert_eq!(
            derive_icon_state(&snap_with("blanked", false)),
            IconState::Attention
        );
    }
}
