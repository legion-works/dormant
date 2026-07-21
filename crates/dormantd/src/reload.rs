//! Config-file watching for hot reload.
//!
//! The actual rebuild lives in the app's run loop (`Runner::reload`); this
//! module only sets up the filesystem watcher that pokes the run loop when the
//! config file changes. SIGHUP is wired directly in the run loop, and rapid
//! change bursts are coalesced by a debounce window (`daemon.reload_debounce`).
//!
//! ## Reload semantics (v1)
//!
//! A reload validates and assembles the **new** config first; an invalid
//! config only flags `pending_reload` on the live engine (no teardown). A
//! valid config triggers a restart-in-place. Because the rebuilt state
//! machines start `Active`, retained displays that were physically dark before
//! the reload receive a **defensive physical wake** (`reload_defensive_wake`)
//! so an occupied room is never left dark waiting for the next sensor edge.
//! This is a sanctioned v1 limitation: it can cause a brief wake-flash on a
//! display that should have stayed blanked; the next absent edge re-blanks it.
//!
//! We watch the config file's **parent directory** (not the file inode)
//! because editors and `install(1)` frequently replace the file via
//! rename, which detaches an inode-level watch. Events are filtered down to
//! the config path and coalesced into a unit tick.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dormant_core::config::schema::{Config, DisplayConfig};
use dormant_core::rules::StateSnapshot;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// A live config watcher. Dropping it stops watching.
pub struct ConfigWatcher {
    /// Held to keep the OS watch alive for the process lifetime.
    _watcher: notify::RecommendedWatcher,
    /// Ticks (one per relevant filesystem change).
    pub rx: mpsc::Receiver<()>,
}

/// Start watching `config_path` for modify/create events.
///
/// # Errors
///
/// Returns an error if the watcher cannot be created or the parent directory
/// cannot be watched.
pub fn config_watcher(config_path: &Path) -> Result<ConfigWatcher> {
    let (tx, rx) = mpsc::channel(8);

    let target: PathBuf = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let target_name = config_path.file_name().map(std::ffi::OsString::from);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        // Fire only for the file we care about. If the event carries no
        // paths, fire anyway (better a spurious reload than a missed one).
        let relevant = event.paths.is_empty()
            || event.paths.iter().any(|p| {
                p == &target
                    || (target_name.is_some()
                        && p.file_name().map(std::ffi::OsString::from) == target_name)
            });
        if relevant {
            let _ = tx.blocking_send(());
        }
    })
    .context("create config watcher")?;

    let watch_dir = config_path.parent().filter(|p| !p.as_os_str().is_empty());
    let watch_dir = watch_dir.unwrap_or_else(|| Path::new("."));
    watcher
        .watch(watch_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch config directory '{}'", watch_dir.display()))?;

    Ok(ConfigWatcher {
        _watcher: watcher,
        rx,
    })
}

// ── Reload carry-over: dispatch-relevant voiding gate ──────────────────────────
//
// A reload seeds `wake_attempts`/`last_blank_failed` forward from the
// pre-reload snapshot (see `app::apply_restore`) so recorded wake-failure
// evidence survives a config hot-reload instead of silently resetting.  But
// that evidence describes the outcome of a specific blank/wake COMMAND; if
// the edit that triggered the reload changed what command gets sent for a
// display (a new `wake_command`, a different controller chain, …) the old
// evidence no longer describes anything real and must be voided rather than
// carried forward under a misleading "still failing" (or "recovered") label.
//
// `dispatch_relevant_eq` draws that line field-by-field; `zero_changed_displays`
// applies it across a whole snapshot at reload time.

/// True when the fields that determine blank/wake DISPATCH OUTCOME are equal
/// between two [`DisplayConfig`]s.
///
/// This is an EXHAUSTIVE destructure (spec R3-M6 drift guard): adding a new
/// `DisplayConfig` field is a compile error here until it is explicitly
/// classified as dispatch-relevant (bound and compared below) or cosmetic
/// (bound to `_`, with a comment explaining why it cannot change blank/wake
/// command construction or controller-chain membership).
///
/// Ignored (cosmetic) fields and why:
/// - `screensaver`: feeds the render overlay only, never the blank/wake
///   command or controller chain.
/// - `restore_brightness`, `samsung_restore_backlight`: post-wake cosmetic
///   restoration values, consumed after dispatch succeeds — never part of
///   command construction.
/// - `panel_type`: wear-heuristic classification (oled-health feature); did
///   not exist when this plan was written, and is cosmetic for dispatch —
///   it only steers wear tracking, never blank/wake command construction.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn dispatch_relevant_eq(a: &DisplayConfig, b: &DisplayConfig) -> bool {
    // Keep this field classification in sync with
    // `dormant_displays::registry::controller_chain_fingerprint`: both reload
    // carry-over and blank-owner retention must agree on dispatch changes.
    let DisplayConfig {
        controllers: a_controllers,
        blank_mode: a_blank_mode,
        degraded_mode: a_degraded_mode,
        ladder: a_ladder,
        screensaver: _a_screensaver,
        output: a_output,
        ddc_display: a_ddc_display,
        host: a_host,
        wol_mac: a_wol_mac,
        blank_command: a_blank_command,
        wake_command: a_wake_command,
        modes: a_modes,
        ha_url: a_ha_url,
        blank_service: a_blank_service,
        blank_data: a_blank_data,
        wake_service: a_wake_service,
        wake_data: a_wake_data,
        command_timeout: a_command_timeout,
        restore_brightness: _a_restore_brightness,
        samsung_restore_backlight: _a_samsung_restore_backlight,
        treat_unreachable_as_blanked: a_treat_unreachable_as_blanked,
        panel_type: _a_panel_type,
        ..
    } = a;
    let DisplayConfig {
        controllers: b_controllers,
        blank_mode: b_blank_mode,
        degraded_mode: b_degraded_mode,
        ladder: b_ladder,
        screensaver: _b_screensaver,
        output: b_output,
        ddc_display: b_ddc_display,
        host: b_host,
        wol_mac: b_wol_mac,
        blank_command: b_blank_command,
        wake_command: b_wake_command,
        modes: b_modes,
        ha_url: b_ha_url,
        blank_service: b_blank_service,
        blank_data: b_blank_data,
        wake_service: b_wake_service,
        wake_data: b_wake_data,
        command_timeout: b_command_timeout,
        restore_brightness: _b_restore_brightness,
        samsung_restore_backlight: _b_samsung_restore_backlight,
        treat_unreachable_as_blanked: b_treat_unreachable_as_blanked,
        panel_type: _b_panel_type,
        ..
    } = b;

    a_controllers == b_controllers
        && a_blank_mode == b_blank_mode
        && a_degraded_mode == b_degraded_mode
        && a_ladder == b_ladder
        && a_output == b_output
        && a_ddc_display == b_ddc_display
        && a_host == b_host
        && a_wol_mac == b_wol_mac
        && a_blank_command == b_blank_command
        && a_wake_command == b_wake_command
        && a_modes == b_modes
        && a_ha_url == b_ha_url
        && a_blank_service == b_blank_service
        && a_blank_data == b_blank_data
        && a_wake_service == b_wake_service
        && a_wake_data == b_wake_data
        && a_command_timeout == b_command_timeout
        && a_treat_unreachable_as_blanked == b_treat_unreachable_as_blanked
}

/// Clone of `snapshot` with `wake_attempts`/`last_blank_failed` zeroed for
/// every display whose dispatch-relevant config differs between `old` and
/// `new` (see [`dispatch_relevant_eq`]).
///
/// A display present in only one of `old`/`new` (added or removed by the
/// edit that triggered this reload) is treated as CHANGED — there is no
/// meaningful "same dispatch" baseline to compare against, so the
/// conservative choice is to zero rather than risk carrying stale evidence
/// forward under a new (or now-absent) configuration.
#[must_use]
pub fn zero_changed_displays(
    snapshot: &StateSnapshot,
    old: &Config,
    new: &Config,
) -> StateSnapshot {
    let mut out = snapshot.clone();
    for (id, dsnap) in &mut out.displays {
        let changed = match (old.displays.get(id), new.displays.get(id)) {
            (Some(o), Some(n)) => !dispatch_relevant_eq(o, n),
            // Added/removed — no baseline to compare against; treat as changed.
            _ => true,
        };
        if changed {
            dsnap.wake_attempts = 0;
            dsnap.last_blank_failed = false;
        }
    }
    out
}

// ── Reload carry-over: sensor `reported` voiding gate ───────────────────────────
//
// `reported` (see `SensorSnapshot::reported` / `RulesEngine::seed_sensor_reported`)
// is a diagnostic bit meaning "this sensor has delivered at least one real
// event since daemon start". A reload seeds it forward from the pre-reload
// snapshot (see `app::apply_restore`) so the diagnostic survives a hot-reload
// instead of silently resetting to the fail-safe `false`. But if the edit
// that triggered the reload changed the sensor's OWN configuration (topic,
// kind, hold time, stale timeout, …) the carried-forward `true` no longer
// describes anything meaningful about the new sensor identity, and must be
// voided.
//
// Unlike `dispatch_relevant_eq` (per-field classification for
// `DisplayConfig`), this gate is deliberately coarse: it compares the WHOLE
// `SensorConfig` and zeroes `reported` on ANY difference — there is no field
// on a sensor exempted as "cosmetic". This intentionally also zeroes on a
// `stale_timeout`-only tweak (spec F11: accepted false-void, pinned by
// `stale_timeout_only_tweak_zeroes_reported` below) rather than trying to
// draw a dispatch-relevance line for sensors.

/// Clone of `snapshot` with `reported` zeroed for every sensor whose whole
/// [`dormant_core::config::schema::SensorConfig`] differs between `old` and
/// `new`.
///
/// A sensor present in only one of `old`/`new` (added or removed by the edit
/// that triggered this reload) is treated as CHANGED — there is no baseline
/// to compare against, so the conservative choice is to zero rather than
/// carry a stale "has reported" bit forward under a new (or now-absent)
/// configuration (spec R3-S).
#[must_use]
pub fn zero_changed_sensor_reported(
    snapshot: &StateSnapshot,
    old: &Config,
    new: &Config,
) -> StateSnapshot {
    let mut out = snapshot.clone();
    for ssnap in &mut out.sensors {
        if old.sensors.get(ssnap.id.as_str()) != new.sensors.get(ssnap.id.as_str()) {
            ssnap.reported = false;
        }
    }
    out
}

#[cfg(test)]
mod dispatch_gate_tests {
    use std::time::Duration;

    use dormant_core::config::schema::{
        AudioConfig, DaemonConfig, NotificationsConfig, ScreensaverConfig, SensorConfig,
        WatchdogConfig, WearConfig,
    };
    use dormant_core::rules::{DisplaySnapshot, SensorSnapshot};
    use dormant_core::types::SensorState;
    use dormant_core::wear::PanelType;
    use indexmap::IndexMap;

    use super::*;

    /// A minimal `DisplayConfig` test builder (P11) — every field set to a
    /// concrete, non-default-looking value where practical so mutation tests
    /// have something to actually change.
    fn base_display_cfg() -> DisplayConfig {
        DisplayConfig {
            scope: dormant_core::config::DisplayScope::default(),
            shared_input_code: None,
            controllers: vec!["ddcci".into()],
            blank_mode: Some(dormant_core::types::BlankMode::PowerOff),
            degraded_mode: None,
            ladder: Vec::new(),
            screensaver: None,
            output: Some("DP-1".into()),
            ddc_display: Some("1-1".into()),
            host: None,
            wol_mac: None,
            blank_command: Some("printf B".into()),
            wake_command: Some("printf W".into()),
            modes: Some(vec![dormant_core::types::BlankMode::PowerOff]),
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: Duration::from_secs(5),
            restore_brightness: 100,
            samsung_restore_backlight: dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: PanelType::default(),
        }
    }

    /// A minimal `ScreensaverConfig` for exercising the cosmetic `screensaver`
    /// field.
    fn test_screensaver() -> ScreensaverConfig {
        ScreensaverConfig {
            trigger: "vacancy".into(),
            audio: false,
            source: Vec::new(),
            scale_mode: None,
            transition: None,
            transition_duration: None,
            shift_px: 4,
            shift_interval: Duration::from_secs(60),
        }
    }

    fn config_with_displays(displays: Vec<(&str, DisplayConfig)>) -> Config {
        let mut map = IndexMap::new();
        for (id, dc) in displays {
            map.insert(id.to_string(), dc);
        }
        Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: map,
            rules: IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
        }
    }

    fn display_snapshot(wake_attempts: u64, last_blank_failed: bool) -> DisplaySnapshot {
        DisplaySnapshot {
            phase: "active".into(),
            inhibited: false,
            paused: false,
            cmd_gen: 0,
            controllers: Vec::new(),
            wake_attempts,
            last_blank_failed,
            stage: None,
        }
    }

    fn snapshot_with_displays(displays: Vec<(&str, u64, bool)>) -> StateSnapshot {
        StateSnapshot {
            sensors: Vec::new(),
            zones: Vec::new(),
            displays: displays
                .into_iter()
                .map(|(id, attempts, failed)| (id.to_string(), display_snapshot(attempts, failed)))
                .collect(),
            pending_reload: None,
            rollback: None,
        }
    }

    #[test]
    fn cosmetic_edit_is_dispatch_equal() {
        let a = base_display_cfg();
        let mut b = a.clone();
        b.restore_brightness = 55; // cosmetic
        b.screensaver = Some(test_screensaver()); // cosmetic
        assert!(
            dispatch_relevant_eq(&a, &b),
            "cosmetic edits must NOT void failure evidence"
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn dispatch_edit_is_not_equal() {
        let a = base_display_cfg();
        let mutations: Vec<Box<dyn Fn(&mut DisplayConfig)>> = vec![
            Box::new(|c: &mut DisplayConfig| c.wake_command = Some("newcmd".into())),
            Box::new(|c: &mut DisplayConfig| c.controllers = vec!["kwin-dpms".into()]),
            Box::new(|c: &mut DisplayConfig| c.host = Some("192.0.2.9".into())),
        ];
        for mutate in &mutations {
            let mut b = a.clone();
            mutate(&mut b);
            assert!(!dispatch_relevant_eq(&a, &b));
        }
    }

    #[test]
    fn zero_changed_displays_zeroes_only_changed() {
        let old_cfg =
            config_with_displays(vec![("m", base_display_cfg()), ("tv", base_display_cfg())]);
        let mut new_cfg = old_cfg.clone();
        new_cfg.displays.get_mut("tv").unwrap().wake_command = Some("newcmd".into());

        let snap = snapshot_with_displays(vec![("m", 5, true), ("tv", 4, true)]);

        let out = zero_changed_displays(&snap, &old_cfg, &new_cfg);

        let m = out
            .displays
            .iter()
            .find(|(id, _)| id == "m")
            .expect("m present");
        assert_eq!(m.1.wake_attempts, 5, "unchanged display must carry over");
        assert!(m.1.last_blank_failed, "unchanged display must carry over");

        let tv = out
            .displays
            .iter()
            .find(|(id, _)| id == "tv")
            .expect("tv present");
        assert_eq!(tv.1.wake_attempts, 0, "changed display must be zeroed");
        assert!(!tv.1.last_blank_failed, "changed display must be zeroed");
    }

    #[test]
    fn zero_changed_displays_zeroes_added_and_removed() {
        let old_cfg = config_with_displays(vec![("m", base_display_cfg())]);
        // "new" removes "m" is not directly observable from a snapshot that
        // already only contains "m"/"gone", but we exercise both directions:
        // "gone" is absent from `new` (removed) and "added" is absent from
        // `old` (added) — both have no baseline and must zero.
        let new_cfg = config_with_displays(vec![("added", base_display_cfg())]);

        let snap = snapshot_with_displays(vec![("m", 3, true), ("added", 2, true)]);
        let out = zero_changed_displays(&snap, &old_cfg, &new_cfg);

        for (id, dsnap) in &out.displays {
            assert_eq!(dsnap.wake_attempts, 0, "{id} must be zeroed (no baseline)");
            assert!(
                !dsnap.last_blank_failed,
                "{id} must be zeroed (no baseline)"
            );
        }
    }

    // ── Sensor `reported` voiding gate (P4 sensor-side test helpers) ────────

    /// A minimal one-sensor [`StateSnapshot`] with `reported` set as
    /// requested.
    fn snap_with_reported(id: &str, reported: bool) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![SensorSnapshot {
                id: id.to_string(),
                state: SensorState::Present,
                last_seen_secs_ago: 0,
                reported,
            }],
            zones: Vec::new(),
            displays: Vec::new(),
            pending_reload: None,
            rollback: None,
        }
    }

    /// Look up a sensor by id in a snapshot (test helper).
    fn sensor<'a>(snap: &'a StateSnapshot, id: &str) -> &'a SensorSnapshot {
        snap.sensors
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("sensor '{id}' not present in snapshot"))
    }

    /// A minimal one-mqtt-sensor `Config`, built via TOML text run through
    /// [`dormant_core::config::load_config`] (rather than hand-constructed)
    /// so the sensor-side tests exercise the same parse path production
    /// config goes through.
    fn cfg_from_toml(toml_str: &str) -> Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml_str).expect("write test config");
        let (cfg, _warnings) =
            dormant_core::config::load_config(&path, dormant_core::config::Strictness::Warn)
                .expect("load test config");
        cfg
    }

    /// One `mqtt` sensor named "desk".
    fn cfg_a() -> Config {
        cfg_from_toml(
            r#"config_version = 1

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "zigbee2mqtt/desk"
"#,
        )
    }

    /// Rewrite sensor `id`'s mqtt `topic` in place (test mutator).
    fn set_mqtt_topic(cfg: &mut Config, id: &str, topic: &str) {
        if let Some(SensorConfig::Mqtt(m)) = cfg.sensors.get_mut(id) {
            m.topic = topic.to_string();
        }
    }

    /// Rewrite sensor `id`'s mqtt `stale_timeout` in place (test mutator).
    fn set_mqtt_stale_timeout(cfg: &mut Config, id: &str, timeout: Duration) {
        if let Some(SensorConfig::Mqtt(m)) = cfg.sensors.get_mut(id) {
            m.stale_timeout = Some(timeout);
        }
    }

    /// `cfg_a()` with sensor `id` removed entirely (spec R3-S: added/removed
    /// sensors have no baseline and must be treated as changed).
    fn cfg_without(id: &str) -> Config {
        let mut cfg = cfg_a();
        cfg.sensors.shift_remove(id);
        cfg
    }

    #[test]
    fn unchanged_sensor_carries_reported() {
        let out =
            zero_changed_sensor_reported(&snap_with_reported("desk", true), &cfg_a(), &cfg_a());
        assert!(sensor(&out, "desk").reported);
    }

    #[test]
    fn changed_sensor_config_zeroes_reported() {
        let mut new = cfg_a();
        set_mqtt_topic(&mut new, "desk", "zigbee2mqtt/desk-NEW");
        assert!(
            !sensor(
                &zero_changed_sensor_reported(&snap_with_reported("desk", true), &cfg_a(), &new),
                "desk"
            )
            .reported
        );
    }

    #[test]
    fn sensor_absent_from_new_zeroed_by_generic_path_never_seeded() {
        // spec R3-S
        let new = cfg_without("desk");
        let out = zero_changed_sensor_reported(&snap_with_reported("desk", true), &cfg_a(), &new);
        assert!(!sensor(&out, "desk").reported); // Some != None -> zeroed
    }

    #[test]
    fn stale_timeout_only_tweak_zeroes_reported() {
        // spec F11 accepted false-void, pinned
        let mut new = cfg_a();
        set_mqtt_stale_timeout(&mut new, "desk", Duration::from_secs(999));
        assert!(
            !sensor(
                &zero_changed_sensor_reported(&snap_with_reported("desk", true), &cfg_a(), &new),
                "desk"
            )
            .reported
        );
    }
}
