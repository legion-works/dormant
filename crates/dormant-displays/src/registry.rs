//! Static display-controller registry — no macro magic (AGENTS.md rule 4).
//!
//! Each controller type registers itself here. The registry is the single
//! source of truth for:
//!
//! - the set of valid `controllers` entries in [`DisplayConfig`] (via
//!   [`CONTROLLER_TYPES`]),
//! - the *static* candidate modes per controller (via [`capabilities`]) —
//!   the config-validate layer-1 check uses this to assert that a config
//!   asking for `kwin-dpms` with `degraded_mode = "screen_off_audio_on"`
//!   isn't asking the impossible. The `command` controller has an empty
//!   static capability set because its modes are declared in the config
//!   (different hardware — different modes); `capabilities()` returns an
//!   empty vec for it and the per-display `modes` array is what fills in.
//! - the per-display chain assembly (via [`build_controllers`]).

use std::collections::HashMap;
use std::sync::Arc;

use dormant_core::config::schema::{Credentials, DisplayConfig};
use dormant_core::error::DormantError;
use dormant_core::traits::DisplayController;
use dormant_core::types::BlankMode;

use crate::command::CommandController;
use crate::ddc_lock::PanelLocks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::ddcci::DdcciController;
use crate::ha_passthrough::HaPassthroughController;
#[cfg(target_os = "linux")]
use crate::kwin_dpms::KwinDpmsController;
use crate::samsung_tizen::SamsungTizenController;

/// Every `DisplayConfig.controllers[]` entry MUST be one of these literals.
///
/// Entries are platform-gated:
/// - `ddcci` (DDC/CI, controller-name-stable across backends — see
///   `crate::ddcci` module docs) is available on Linux (I²C-dev) and macOS
///   (the vendored `ddc-macos` fork).
/// - `kwin-dpms` is Linux-only (a KDE Plasma / `KWin` compositor feature).
///
/// Tasks 12-15 append additional entries (`KWin` DPMS, Samsung Tizen,
/// LG webOS, HA passthrough, …) as their modules land.
///
/// Tests: `controller_types_contains_ddcci_on_linux` /
/// `controller_types_contains_ddcci_on_macos` pin presence on their
/// respective targets; `controller_types_excludes_ddcci_elsewhere` pins
/// absence everywhere else, so config validation rejects `ddcci`
/// deterministically on a platform that can never build `RealVcp`.
#[cfg(target_os = "linux")]
pub const CONTROLLER_TYPES: &[&str] = &[
    "command",
    "ddcci",
    "ha-passthrough",
    "kwin-dpms",
    "samsung-tizen",
];
#[cfg(target_os = "macos")]
pub const CONTROLLER_TYPES: &[&str] = &["command", "ddcci", "ha-passthrough", "samsung-tizen"];
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub const CONTROLLER_TYPES: &[&str] = &["command", "ha-passthrough", "samsung-tizen"];

/// Static candidate modes per controller type.
///
/// Returned shape:
/// - `command` → empty vec — modes are declared in the per-display config
///   (`DisplayConfig.modes`) because the shell command's behavior depends on
///   the user's hardware.
///
/// `capabilities()` is the single grep-stable source for config-validate
/// layer-1 checks (does the user's `blank_mode` / `degraded_mode` make sense
/// for the controllers it asks for?).
///
/// `ddcci` is listed on Linux and macOS (see [`CONTROLLER_TYPES`] for the
/// platform rationale); `kwin-dpms` remains Linux-only.
#[must_use]
pub fn capabilities() -> HashMap<String, Vec<BlankMode>> {
    let mut m: HashMap<String, Vec<BlankMode>> = HashMap::new();
    m.insert("command".to_string(), Vec::new());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    m.insert(
        "ddcci".to_string(),
        vec![BlankMode::BrightnessZero, BlankMode::PowerOff],
    );
    #[cfg(target_os = "linux")]
    m.insert("kwin-dpms".to_string(), vec![BlankMode::PowerOff]);
    m.insert("ha-passthrough".to_string(), Vec::new());
    m.insert(
        "samsung-tizen".to_string(),
        vec![
            BlankMode::ScreenOffAudioOn,
            BlankMode::BrightnessZero,
            BlankMode::PowerOff,
        ],
    );
    m
}

/// Build the ordered controller chain for one display.
///
/// # Errors
///
/// - [`DormantError::ConfigInvalid`] if a controller name in
///   `cfg.controllers` is unknown to [`CONTROLLER_TYPES`].
/// - [`DormantError::ConfigInvalid`] if a `command` entry is missing
///   `blank_command`, `wake_command`, or `modes` (the error names the
///   display so the operator can locate it in their TOML). An *empty*
///   `modes = []` is treated the same as a missing `modes` — an empty
///   capability set can never blank any mode.
///
/// `locks` is the shared per-panel DDC/CI lock registry (spec §4.3) — the
/// caller constructs exactly ONE `Arc<PanelLocks>` for its whole lifetime
/// (the daemon in `App::start`; each direct-hardware CLI invocation gets
/// its own fresh one, being a separate process) and passes it to every
/// `build_controllers` call, so that a panel's lock resolves to the same
/// `Arc<PanelLock>` no matter which config-reload generation or call site
/// derived its controller. Only the `ddcci` arm consumes it; every other
/// controller type ignores the parameter (no shared physical bus to
/// serialize).
#[allow(clippy::too_many_lines)]
pub fn build_controllers(
    display_name: &str,
    cfg: &DisplayConfig,
    creds: &Credentials,
    locks: &Arc<PanelLocks>,
) -> Result<Vec<Box<dyn DisplayController>>, DormantError> {
    let mut chain: Vec<Box<dyn DisplayController>> = Vec::with_capacity(cfg.controllers.len());

    for name in &cfg.controllers {
        match name.as_str() {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            "ddcci" => {
                // Normalize empty matcher to None so the controller auto-selects
                // the single detected display instead of trying to match "".
                let matcher = cfg.ddc_display.clone().filter(|s| !s.is_empty());
                chain.push(Box::new(DdcciController::new(
                    matcher,
                    cfg.restore_brightness,
                    cfg.primary_blank_mode(),
                    locks,
                )));
            }
            #[cfg(target_os = "linux")]
            "kwin-dpms" => {
                chain.push(Box::new(KwinDpmsController::new(
                    cfg.output.clone(),
                    cfg.command_timeout,
                )));
            }
            "command" => {
                let blank_command = cfg
                    .blank_command
                    .as_ref()
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing 'blank_command'"))?;
                let wake_command = cfg
                    .wake_command
                    .as_ref()
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing 'wake_command'"))?;
                // Treat `modes = Some(vec![])` the same as `modes = None`:
                // an empty capability set can never blank any mode, so the
                // configuration is structurally broken. The dormant-core
                // `validate_display` produces a "blank-incapable display"
                // ValidationError for the same condition; we surface it here
                // as a hard ConfigInvalid so the daemon refuses to start.
                let modes = cfg
                    .modes
                    .as_ref()
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing or empty 'modes'"))?;

                chain.push(Box::new(CommandController::new(
                    blank_command.clone(),
                    wake_command.clone(),
                    modes.clone(),
                    cfg.command_timeout,
                )));
            }
            "ha-passthrough" => {
                let ha_url = cfg
                    .ha_url
                    .as_ref()
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing 'ha_url'"))?;
                let blank_service = cfg
                    .blank_service
                    .as_ref()
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing 'blank_service'"))?;
                let wake_service = cfg
                    .wake_service
                    .as_ref()
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing 'wake_service'"))?;
                let modes = cfg
                    .modes
                    .as_ref()
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing or empty 'modes'"))?;
                let token = creds
                    .ha_token
                    .as_ref()
                    .ok_or_else(|| DormantError::CredsMissing {
                        what: format!(
                            "display '{display_name}': ha-passthrough requires 'ha_token' \
                             in credentials file"
                        ),
                    })?;

                chain.push(Box::new(HaPassthroughController::new(
                    ha_url.clone(),
                    token.clone(),
                    blank_service.as_str(),
                    cfg.blank_data.clone(),
                    wake_service.as_str(),
                    cfg.wake_data.clone(),
                    modes.clone(),
                )?));
            }
            "samsung-tizen" => {
                let host = cfg
                    .host
                    .as_ref()
                    .ok_or_else(|| config_invalid_cmd(display_name, "missing 'host'"))?;
                let token = creds
                    .samsung
                    .get(host)
                    .ok_or_else(|| DormantError::CredsMissing {
                        what: format!(
                            "display '{display_name}': samsung-tizen requires a token for \
                             host '{host}' in credentials file (key: samsung.{host})"
                        ),
                    })?;

                chain.push(Box::new(SamsungTizenController::new(
                    host.clone(),
                    token.clone(),
                    cfg.wol_mac.clone(),
                    cfg.treat_unreachable_as_blanked,
                    cfg.primary_blank_mode(),
                    cfg.samsung_restore_backlight,
                )));
            }
            other => {
                return Err(DormantError::ConfigInvalid {
                    detail: format!(
                        "display '{display_name}': unknown controller '{other}' \
                         (known: {})",
                        CONTROLLER_TYPES.join(", "),
                    ),
                });
            }
        }
    }

    Ok(chain)
}

fn config_invalid_cmd(display_name: &str, detail: &str) -> DormantError {
    DormantError::ConfigInvalid {
        detail: format!("display '{display_name}': {detail}"),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use dormant_core::config::defaults::{COMMAND_TIMEOUT, SAMSUNG_RESTORE_BACKLIGHT};
    use std::time::Duration;

    /// Minimal valid `command` display config — used by the happy-path test
    /// and as a base for "missing fields" variants.
    fn command_cfg() -> DisplayConfig {
        DisplayConfig {
            controllers: vec!["command".into()],
            blank_mode: Some(BlankMode::PowerOff),
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: Some("/usr/bin/xset dpms force off".into()),
            wake_command: Some("/usr/bin/xset dpms force on".into()),
            modes: Some(vec![BlankMode::PowerOff]),
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: COMMAND_TIMEOUT,
            restore_brightness: 80,
            samsung_restore_backlight: SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
        }
    }

    #[test]
    fn capabilities_has_command_empty() {
        let caps = capabilities();
        let cmd = caps
            .get("command")
            .expect("'command' must be in capabilities");
        assert!(cmd.is_empty(), "command controller has no static modes");
    }

    #[test]
    fn controller_types_contains_command() {
        assert!(CONTROLLER_TYPES.contains(&"command"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn controller_types_contains_ddcci_on_linux() {
        assert!(
            CONTROLLER_TYPES.contains(&"ddcci"),
            "ddcci must be registered on Linux"
        );
    }

    /// Task 6: `ddcci` broadens to macOS (the vendored `ddc-macos` fork
    /// backs `RealVcp` there — see `vendor/ddc-macos/README.dormant.md`).
    /// DEFERRED: PR CI — `#[cfg(target_os = "macos")]` code never compiles
    /// in this Linux sandbox, so this cannot run here; it is written now so
    /// the macOS CI lane (Task 2) exercises it.
    #[test]
    #[cfg(target_os = "macos")]
    fn controller_types_contains_ddcci_on_macos() {
        assert!(
            CONTROLLER_TYPES.contains(&"ddcci"),
            "ddcci must be registered on macOS"
        );
    }

    // Everywhere else (e.g. Windows): ddcci is deliberately absent from
    // CONTROLLER_TYPES so that config validation rejects
    // `controllers = ["ddcci"]` deterministically with "unknown controller"
    // rather than silently accepting it and failing later at controller
    // build time — there is no `RealVcp` backend for these targets.
    #[test]
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn controller_types_excludes_ddcci_elsewhere() {
        assert!(
            !CONTROLLER_TYPES.contains(&"ddcci"),
            "ddcci must NOT be registered on platforms with no RealVcp backend"
        );
    }

    /// Task 6 RED-first test 1: `CONTROLLER_TYPES` and `capabilities()`
    /// must advertise EXACTLY the same set of controller type names for
    /// whichever target this crate is compiled for — a drift here means
    /// config validation (`CONTROLLER_TYPES`) and the layer-1 blank-mode
    /// check (`capabilities()`) disagree about what a controller name
    /// means. Runs unconditionally (no target cfg) since the property must
    /// hold on every platform, not just Linux; on Linux specifically it was
    /// already true before this task's changes — see the report for the
    /// "already green" note.
    #[test]
    fn advertised_types_exactly_match_capabilities() {
        use std::collections::BTreeSet;

        let types: BTreeSet<String> = CONTROLLER_TYPES.iter().map(|s| (*s).to_string()).collect();
        let caps: BTreeSet<String> = capabilities().into_keys().collect();
        assert_eq!(
            types, caps,
            "CONTROLLER_TYPES and capabilities() must advertise exactly the \
             same controller-type names for this compiled target: \
             CONTROLLER_TYPES={types:?} capabilities()={caps:?}"
        );
    }

    /// Task 6 RED-first test 2: macOS advertises `ddcci` with EXACTLY
    /// `[PowerOff, BrightnessZero]` — the same static capability set as
    /// Linux, since it is the same controller (not a parallel macOS-only
    /// controller) backed by the same `ddc-hi`/`RealVcp` code path.
    /// DEFERRED: PR CI — cannot run in this Linux sandbox; written now for
    /// the macOS CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_advertises_ddcci() {
        use std::collections::HashSet;

        let caps = capabilities();
        let ddcci_modes: HashSet<BlankMode> = caps
            .get("ddcci")
            .expect("ddcci must be present in capabilities() on macOS")
            .iter()
            .copied()
            .collect();
        let expected: HashSet<BlankMode> = [BlankMode::PowerOff, BlankMode::BrightnessZero]
            .into_iter()
            .collect();
        assert_eq!(
            ddcci_modes, expected,
            "macOS ddcci capabilities must be exactly [PowerOff, BrightnessZero]"
        );
    }

    #[test]
    fn build_command_happy() {
        let cfg = command_cfg();
        let creds = Credentials::default();
        let chain = build_controllers("main", &cfg, &creds, &PanelLocks::new()).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "command");
        assert_eq!(chain[0].supported_modes(), vec![BlankMode::PowerOff]);
    }

    /// A Samsung display wired with `blank_mode = None` and a ladder whose
    /// first `Controller` stage is `BrightnessZero` — `DisplayConfig::primary_blank_mode()`
    /// returns `BrightnessZero`, and the registry must thread that into
    /// `SamsungTizenController::configured_primary_mode`. Pre-fix wiring
    /// (`cfg.blank_mode.unwrap_or(ScreenOffAudioOn)`) built the controller
    /// as `ScreenOffAudioOn` and wake fell through to `KEY_RETURN`, leaving
    /// the panel dim after a backlight blank. This test pins the registry
    /// path end-to-end — bypassed by the direct-construction test in
    /// `samsung_tizen.rs::ladder_primary_brightness_zero_wake_restores_backlight_after_restart`,
    /// which only exercises `SamsungTizenController::wake()` in isolation.
    #[test]
    fn build_samsung_ladder_primary_brightness_zero_wires_configured_primary_mode() {
        use crate::samsung_tizen::SamsungTizenController;
        use dormant_core::types::{LadderStage, StageKind};

        let cfg = DisplayConfig {
            controllers: vec!["samsung-tizen".into()],
            blank_mode: None,
            degraded_mode: None,
            ladder: vec![LadderStage {
                kind: StageKind::Controller(BlankMode::BrightnessZero),
                dwell: None,
            }],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: Some("192.0.2.7".into()),
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: COMMAND_TIMEOUT,
            restore_brightness: 80,
            samsung_restore_backlight: 42, // operator-tuned override
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
        };
        let mut creds = Credentials::default();
        creds
            .samsung
            .insert("192.0.2.7".into(), "test-token".into());

        let chain = build_controllers("tv", &cfg, &creds, &PanelLocks::new()).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "samsung-tizen");

        // Downcast to inspect the registry-wired controller. `DisplayController`
        // is `Any` so the trait object can be coerced to `Box<dyn Any>` for
        // downcasting — without exposing test-only methods on the trait.
        let boxed = chain.into_iter().next().expect("one controller");
        let ctrl: Box<SamsungTizenController> = (boxed as Box<dyn std::any::Any>)
            .downcast()
            .expect("samsung-tizen controller downcast");

        assert_eq!(
            ctrl.configured_primary_mode(),
            BlankMode::BrightnessZero,
            "registry must thread DisplayConfig::primary_blank_mode() — \
             which walks the ladder and finds the BrightnessZero stage — \
             into the controller's configured_primary_mode field. \
             Pre-fix `cfg.blank_mode.unwrap_or(ScreenOffAudioOn)` would \
             leave it as ScreenOffAudioOn and the wake() path would send \
             KEY_RETURN instead of restoring port-1516 backlight."
        );
        assert_eq!(
            ctrl.restore_backlight_for_test(),
            42,
            "registry must thread the per-display samsung_restore_backlight override"
        );
    }

    /// A DDC/CI display wired with `blank_mode = None` and a ladder whose
    /// first `Controller` stage is `PowerOff` — the registry must thread
    /// `DisplayConfig::primary_blank_mode()` into
    /// `DdcciController::configured_primary_mode` (mirrors
    /// `build_samsung_ladder_primary_brightness_zero_wires_configured_primary_mode`
    /// above). Pre-fix wiring left the field unset (compile error, since the
    /// constructor requires it) — this pins the registry path end-to-end so
    /// a `PowerOff`-primary display wakes via D6-on only before any blank
    /// has run (e.g. right after a daemon restart).
    #[test]
    #[cfg(target_os = "linux")]
    fn build_ddcci_ladder_primary_power_off_wires_configured_primary_mode() {
        use dormant_core::types::{LadderStage, StageKind};

        let mut cfg = command_cfg();
        cfg.controllers = vec!["ddcci".into()];
        cfg.blank_mode = None;
        cfg.ladder = vec![LadderStage {
            kind: StageKind::Controller(BlankMode::PowerOff),
            dwell: None,
        }];
        cfg.ddc_display = Some("DELL".into());

        let creds = Credentials::default();
        let chain = build_controllers("main", &cfg, &creds, &PanelLocks::new()).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "ddcci");

        let boxed = chain.into_iter().next().expect("one controller");
        let ctrl: Box<DdcciController> = (boxed as Box<dyn std::any::Any>)
            .downcast()
            .expect("ddcci controller downcast");

        assert_eq!(
            ctrl.configured_primary_mode(),
            BlankMode::PowerOff,
            "registry must thread DisplayConfig::primary_blank_mode() into \
             DdcciController::configured_primary_mode"
        );
    }

    #[test]
    fn build_unknown_controller_name_fails() {
        let mut cfg = command_cfg();
        cfg.controllers = vec!["lg-webos".into()]; // not yet registered (M3)
        let creds = Credentials::default();
        let res = build_controllers("main", &cfg, &creds, &PanelLocks::new());
        match res {
            Err(DormantError::ConfigInvalid { detail }) => {
                assert!(detail.contains("unknown controller 'lg-webos'"));
                assert!(detail.contains("display 'main'"));
            }
            Err(other) => panic!("expected ConfigInvalid, got {other:?}"),
            Ok(_) => panic!("expected Err for unknown controller"),
        }
    }

    #[test]
    fn build_command_missing_blank_command_fails_naming_display() {
        let mut cfg = command_cfg();
        cfg.blank_command = None;
        let creds = Credentials::default();
        match build_controllers("tv-corner", &cfg, &creds, &PanelLocks::new()) {
            Err(DormantError::ConfigInvalid { detail }) => {
                assert!(
                    detail.contains("display 'tv-corner'"),
                    "error must name the display: {detail}"
                );
                assert!(detail.contains("missing 'blank_command'"));
            }
            Err(other) => panic!("expected ConfigInvalid, got {other:?}"),
            Ok(_) => panic!("expected Err for missing blank_command"),
        }
    }

    #[test]
    fn build_command_missing_wake_command_fails_naming_display() {
        let mut cfg = command_cfg();
        cfg.wake_command = None;
        let creds = Credentials::default();
        match build_controllers("tv-corner", &cfg, &creds, &PanelLocks::new()) {
            Err(DormantError::ConfigInvalid { detail }) => {
                assert!(detail.contains("missing 'wake_command'"));
                assert!(detail.contains("display 'tv-corner'"));
            }
            Err(other) => panic!("expected ConfigInvalid, got {other:?}"),
            Ok(_) => panic!("expected Err for missing wake_command"),
        }
    }

    #[test]
    fn build_command_missing_modes_fails_naming_display() {
        let mut cfg = command_cfg();
        cfg.modes = None;
        let creds = Credentials::default();
        match build_controllers("tv-corner", &cfg, &creds, &PanelLocks::new()) {
            Err(DormantError::ConfigInvalid { detail }) => {
                assert!(detail.contains("missing or empty 'modes'"));
                assert!(detail.contains("display 'tv-corner'"));
            }
            Err(other) => panic!("expected ConfigInvalid, got {other:?}"),
            Ok(_) => panic!("expected Err for missing modes"),
        }
    }

    #[test]
    fn build_command_empty_modes_fails() {
        // Should 5 — `modes = Some(vec![])` is structurally the same as
        // missing: an empty capability set can never blank any mode.
        let mut cfg = command_cfg();
        cfg.modes = Some(vec![]);
        let creds = Credentials::default();
        match build_controllers("tv-corner", &cfg, &creds, &PanelLocks::new()) {
            Err(DormantError::ConfigInvalid { detail }) => {
                assert!(
                    detail.contains("missing or empty 'modes'"),
                    "error must mention empty modes: {detail}"
                );
                assert!(detail.contains("display 'tv-corner'"));
            }
            Err(other) => panic!("expected ConfigInvalid, got {other:?}"),
            Ok(_) => panic!("expected Err for empty modes"),
        }
    }

    #[test]
    fn build_empty_controllers_returns_empty_chain() {
        let mut cfg = command_cfg();
        cfg.controllers = vec![];
        let creds = Credentials::default();
        let chain = build_controllers("no-display", &cfg, &creds, &PanelLocks::new()).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn build_preserves_chain_order() {
        // Future: tasks 12-15 will let us chain multiple controllers. For
        // now this just verifies the ordering invariant with the single
        // registered type.
        let mut cfg = command_cfg();
        cfg.controllers = vec!["command".into(), "command".into()];
        let creds = Credentials::default();
        let chain = build_controllers("multi", &cfg, &creds, &PanelLocks::new()).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].name(), "command");
        assert_eq!(chain[1].name(), "command");
    }

    #[test]
    fn build_propagates_command_timeout() {
        let mut cfg = command_cfg();
        cfg.command_timeout = Duration::from_secs(42);
        let creds = Credentials::default();
        let chain = build_controllers("with-timeout", &cfg, &creds, &PanelLocks::new()).unwrap();
        // We can't observe the timeout directly through the trait, but the
        // controller built with a non-default timeout must at least not panic
        // and expose the configured mode set.
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].supported_modes(), cfg.modes.unwrap());
    }
}
