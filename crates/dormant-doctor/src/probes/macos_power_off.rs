//! macOS shared-DDC/CI power-off hazard doctor probe (issue #126).
//!
//! On the macOS / DDC/CI / shared-display topology, a `power_off` blank
//! can drop the USB-C link and the panel's USB hub — the panel stops
//! accepting VCP writes, the OSD goes dead, and physical recovery (a
//! power-cycle) may be required. Dormant surfaces this hazard at config
//! load time (see `dormant_core::config::validate::is_macos_power_off_hazard`)
//! and again here, where the doctor can name every affected display
//! independently of the daemon's load-time warning path.
//!
//! The hazard topology (`scope = shared`, first controller = `ddcci`, primary
//! blank mode = `power_off`) is identical to the load-time semantic
//! warning's gate. The doctor probe reuses that pure classifier so the
//! load-time warning and the live probe agree on which displays are
//! hazardous — drift between them would defeat the warning.
//!
//! ## What this probe does
//!
//! For each display that matches the hazard topology and has NOT been
//! acknowledged via `power_off_opt_in = true`, the probe emits a
//! `ProbeResult::fail` with `subject = display id` and the operator-facing
//! detail "`power_off` on this topology risks unrecoverable standby
//! (USB-C link drops)". The probe never blanks or wakes a panel — it is
//! strictly diagnostic.
//!
//! ## Why this is gated
//!
//! The probe code itself compiles on every platform (the un-gated
//! classifier is the only thing that needs to run on Linux CI). The
//! wrapper that emits `ProbeResult`s and is called from `probe_all_offline`
//! is `#[cfg(target_os = "macos")]`-gated, mirroring the other macOS
//! probes in this crate — the hazard is a macOS-specific phenomenon, so
//! the doctor only flags it on macOS hosts.
//!
//! ## Recovery
//!
//! This probe adds NO recovery mechanism. Acknowledging the hazard via
//! `power_off_opt_in = true` silences both the load-time warning and the
//! doctor failure; it does not prevent the underlying USB-C dropout when
//! the panel actually standby-stops. Recovery still requires physical
//! power-cycling the monitor — see `docs/src/displays.md`.

#[cfg(target_os = "macos")]
use crate::types::{ProbeResult, ProbeStatus};
use dormant_core::config::schema::{Config, DisplayConfig};
use dormant_core::config::validate::is_macos_power_off_hazard;

/// Pure, un-gated topology classification. True when the display matches
/// the hazard topology (shared + first controller = `ddcci` + primary
/// blank mode = `power_off`). `power_off_opt_in` is consulted by the
/// probe/warning collectors, NOT by this predicate — callers decide
/// whether to suppress on opt-in.
///
/// Exposed un-gated so the doctor probe, the load-time semantic warning
/// collector, and the web UI's hazard checkbox all classify identically
/// without platform gating. On a non-macOS build this is the only entry
/// point — the probe wrapper below is `cfg(target_os = "macos")`-gated —
/// so we suppress the dead-code warning there; the platform-neutral
/// `#[cfg(test)]` exercises it.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
pub fn hazardous_displays(cfg: &Config) -> impl Iterator<Item = (&str, &DisplayConfig)> + '_ {
    cfg.displays
        .iter()
        .filter(|(_, dc)| is_macos_power_off_hazard(dc) && !dc.power_off_opt_in)
        .map(|(id, dc)| (id.as_str(), dc))
}

/// Probe the macOS shared-DDC/CI power-off hazard topology. One
/// `ProbeResult::fail` per hazardous display that has NOT been
/// acknowledged via `power_off_opt_in = true`; the `subject` field names
/// the affected display so the operator can map the failure back to a
/// specific `[displays.<id>]` block.
///
/// Returns an empty Vec when no display matches the hazard topology, or
/// when every matching display has `power_off_opt_in = true`. The probe
/// is read-only — it never blanks or wakes a panel and never writes the
/// config.
#[cfg(target_os = "macos")]
#[must_use]
pub fn probe_macos_power_off_hazard(cfg: &Config) -> Vec<ProbeResult> {
    hazardous_displays(cfg)
        .map(|(id, _)| ProbeResult {
            name: "macos-power-off-hazard".into(),
            status: ProbeStatus::Fail,
            detail: "power_off on this topology risks unrecoverable standby \
                         (USB-C link drops)"
                .into(),
            category: Some("platform".into()),
            subject: Some(id.to_string()),
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use crate::types::ProbeStatus;
    use dormant_core::config::DisplayScope;
    use indexmap::IndexMap;

    fn display(
        scope: DisplayScope,
        controllers: Vec<&str>,
        blank_mode: Option<&str>,
        power_off_opt_in: bool,
    ) -> DisplayConfig {
        use dormant_core::types::BlankMode;
        DisplayConfig {
            controllers: controllers.into_iter().map(String::from).collect(),
            scope,
            blank_mode: blank_mode.map(|m| match m {
                "power_off" => BlankMode::PowerOff,
                "screen_off_audio_on" => BlankMode::ScreenOffAudioOn,
                "brightness_zero" => BlankMode::BrightnessZero,
                other => panic!("unknown blank_mode literal for test: {other}"),
            }),
            power_off_opt_in,
            ..base_display_cfg()
        }
    }

    fn base_display_cfg() -> DisplayConfig {
        use dormant_core::config::defaults;
        DisplayConfig {
            controllers: Vec::new(),
            scope: DisplayScope::Private,
            shared_input_code: None,
            shared_input_write_code: None,
            shared_peer_input_code: None,
            shared_peer_input_write_code: None,
            hooks: dormant_core::config::HookSlots::default(),
            blank_mode: None,
            degraded_mode: None,
            ladder: Vec::new(),
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: defaults::COMMAND_TIMEOUT,
            restore_brightness: defaults::RESTORE_BRIGHTNESS,
            samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
            power_off_opt_in: false,
        }
    }

    fn cfg_with(displays: Vec<(&'static str, DisplayConfig)>) -> Config {
        let mut map = IndexMap::new();
        for (id, dc) in displays {
            map.insert(id.into(), dc);
        }
        Config {
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: map,
            rules: IndexMap::new(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            coordination: dormant_core::config::schema::CoordinationConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }
    }

    #[test]
    fn hazardous_displays_lists_hazardous_shared_ddcci_power_off() {
        let cfg = cfg_with(vec![(
            "aoc",
            display(
                DisplayScope::Shared,
                vec!["ddcci"],
                Some("power_off"),
                false,
            ),
        )]);
        let hits: Vec<_> = hazardous_displays(&cfg).map(|(id, _)| id).collect();
        assert_eq!(hits, vec!["aoc"]);
    }

    #[test]
    fn hazardous_displays_skips_private_scope() {
        let cfg = cfg_with(vec![(
            "private_panel",
            display(
                DisplayScope::Private,
                vec!["ddcci"],
                Some("power_off"),
                false,
            ),
        )]);
        let hits: Vec<_> = hazardous_displays(&cfg).map(|(id, _)| id).collect();
        assert!(hits.is_empty(), "private panel must not be hazardous");
    }

    #[test]
    fn hazardous_displays_skips_screen_off_audio_on_primary() {
        let cfg = cfg_with(vec![(
            "shared_soft",
            display(
                DisplayScope::Shared,
                vec!["ddcci"],
                Some("screen_off_audio_on"),
                false,
            ),
        )]);
        let hits: Vec<_> = hazardous_displays(&cfg).map(|(id, _)| id).collect();
        assert!(
            hits.is_empty(),
            "screen_off_audio_on primary mode must not be hazardous"
        );
    }

    #[test]
    fn hazardous_displays_skips_ddcci_fallback_only() {
        let cfg = cfg_with(vec![(
            "shared_fallback",
            display(
                DisplayScope::Shared,
                vec!["macos-gamma-black", "ddcci"],
                Some("power_off"),
                false,
            ),
        )]);
        let hits: Vec<_> = hazardous_displays(&cfg).map(|(id, _)| id).collect();
        assert!(
            hits.is_empty(),
            "ddcci as a non-first fallback must not be hazardous"
        );
    }

    #[test]
    fn hazardous_displays_skips_opt_in_true() {
        let cfg = cfg_with(vec![(
            "opted_in",
            display(DisplayScope::Shared, vec!["ddcci"], Some("power_off"), true),
        )]);
        let hits: Vec<_> = hazardous_displays(&cfg).map(|(id, _)| id).collect();
        assert!(
            hits.is_empty(),
            "power_off_opt_in = true must silence hazard"
        );
    }

    #[test]
    fn probe_emits_one_fail_per_hazardous_display_with_subject() {
        // Two hazardous displays + one acknowledged. The probe must emit
        // exactly two ProbeResults, each with a distinct subject and the
        // canonical detail copy. Gated on cfg(target_os = "macos") so we
        // only assert against the build the gate enables.
        #[cfg(target_os = "macos")]
        {
            let cfg = cfg_with(vec![
                (
                    "aoc_main",
                    display(
                        DisplayScope::Shared,
                        vec!["ddcci"],
                        Some("power_off"),
                        false,
                    ),
                ),
                (
                    "aoc_secondary",
                    display(
                        DisplayScope::Shared,
                        vec!["ddcci"],
                        Some("power_off"),
                        false,
                    ),
                ),
                (
                    "aoc_acked",
                    display(DisplayScope::Shared, vec!["ddcci"], Some("power_off"), true),
                ),
            ]);
            let results = probe_macos_power_off_hazard(&cfg);
            assert_eq!(results.len(), 2, "two hazardous displays");
            let subjects: Vec<_> = results
                .iter()
                .map(|r| r.subject.as_deref().unwrap_or(""))
                .collect();
            assert!(subjects.contains(&"aoc_main"));
            assert!(subjects.contains(&"aoc_secondary"));
            assert!(!subjects.contains(&"aoc_acked"));
            for r in &results {
                assert_eq!(r.name, "macos-power-off-hazard");
                assert_eq!(r.status, ProbeStatus::Fail);
                assert_eq!(r.category.as_deref(), Some("platform"));
                assert!(r.detail.contains("USB-C link drops"));
            }
        }
    }

    #[test]
    fn probe_returns_empty_vec_when_no_hazardous_display() {
        #[cfg(target_os = "macos")]
        {
            let cfg = cfg_with(vec![(
                "tame",
                display(
                    DisplayScope::Private,
                    vec!["ddcci"],
                    Some("power_off"),
                    false,
                ),
            )]);
            let results = probe_macos_power_off_hazard(&cfg);
            assert!(results.is_empty());
        }
    }
}
