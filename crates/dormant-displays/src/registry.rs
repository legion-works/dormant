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

use dormant_core::config::schema::{Credentials, DisplayConfig};
use dormant_core::error::DormantError;
use dormant_core::traits::DisplayController;
use dormant_core::types::BlankMode;

use crate::command::CommandController;
#[cfg(target_os = "linux")]
use crate::ddcci::DdcciController;
use crate::ha_passthrough::HaPassthroughController;
#[cfg(target_os = "linux")]
use crate::kwin_dpms::KwinDpmsController;
use crate::samsung_tizen::SamsungTizenController;

/// Every `DisplayConfig.controllers[]` entry MUST be one of these literals.
///
/// Entries are platform-gated: `ddcci` (DDC/CI over I²C) is Linux-only.
///
/// Tasks 12-15 append additional entries (`KWin` DPMS, Samsung Tizen,
/// LG webOS, HA passthrough, …) as their modules land.
///
/// Tests: on Linux, `ddcci` must be present (test: `controller_types_contains_ddcci_on_linux`).
/// Off-Linux, it must be absent so config validation rejects it deterministically.
#[cfg(target_os = "linux")]
pub const CONTROLLER_TYPES: &[&str] = &[
    "command",
    "ddcci",
    "ha-passthrough",
    "kwin-dpms",
    "samsung-tizen",
];
#[cfg(not(target_os = "linux"))]
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
/// `ddcci` is only listed on Linux (DDC/CI requires platform I²C support).
#[must_use]
pub fn capabilities() -> HashMap<String, Vec<BlankMode>> {
    let mut m: HashMap<String, Vec<BlankMode>> = HashMap::new();
    m.insert("command".to_string(), Vec::new());
    #[cfg(target_os = "linux")]
    m.insert(
        "ddcci".to_string(),
        vec![BlankMode::BrightnessZero, BlankMode::PowerOff],
    );
    #[cfg(target_os = "linux")]
    m.insert("kwin-dpms".to_string(), vec![BlankMode::PowerOff]);
    m.insert("ha-passthrough".to_string(), Vec::new());
    m.insert(
        "samsung-tizen".to_string(),
        vec![BlankMode::ScreenOffAudioOn, BlankMode::PowerOff],
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
#[allow(clippy::too_many_lines)]
pub fn build_controllers(
    display_name: &str,
    cfg: &DisplayConfig,
    creds: &Credentials,
) -> Result<Vec<Box<dyn DisplayController>>, DormantError> {
    let mut chain: Vec<Box<dyn DisplayController>> = Vec::with_capacity(cfg.controllers.len());

    for name in &cfg.controllers {
        match name.as_str() {
            #[cfg(target_os = "linux")]
            "ddcci" => {
                // Normalize empty matcher to None so the controller auto-selects
                // the single detected display instead of trying to match "".
                let matcher = cfg.ddc_display.clone().filter(|s| !s.is_empty());
                chain.push(Box::new(DdcciController::new(
                    matcher,
                    cfg.restore_brightness,
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
    use dormant_core::config::defaults::COMMAND_TIMEOUT;
    use std::time::Duration;

    /// Minimal valid `command` display config — used by the happy-path test
    /// and as a base for "missing fields" variants.
    fn command_cfg() -> DisplayConfig {
        DisplayConfig {
            controllers: vec!["command".into()],
            blank_mode: BlankMode::PowerOff,
            degraded_mode: None,
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
            treat_unreachable_as_blanked: true,
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

    // Off-Linux: ddcci is deliberately absent from CONTROLLER_TYPES so that
    // config validation rejects `controllers = ["ddcci"]` deterministically
    // with "unknown controller" rather than silently accepting it and failing
    // later at controller build time.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn controller_types_excludes_ddcci_off_linux() {
        assert!(
            !CONTROLLER_TYPES.contains(&"ddcci"),
            "ddcci must NOT be registered on non-Linux platforms"
        );
    }

    #[test]
    fn build_command_happy() {
        let cfg = command_cfg();
        let creds = Credentials::default();
        let chain = build_controllers("main", &cfg, &creds).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "command");
        assert_eq!(chain[0].supported_modes(), vec![BlankMode::PowerOff]);
    }

    #[test]
    fn build_unknown_controller_name_fails() {
        let mut cfg = command_cfg();
        cfg.controllers = vec!["lg-webos".into()]; // not yet registered (M3)
        let creds = Credentials::default();
        let res = build_controllers("main", &cfg, &creds);
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
        match build_controllers("tv-corner", &cfg, &creds) {
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
        match build_controllers("tv-corner", &cfg, &creds) {
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
        match build_controllers("tv-corner", &cfg, &creds) {
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
        match build_controllers("tv-corner", &cfg, &creds) {
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
        let chain = build_controllers("no-display", &cfg, &creds).unwrap();
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
        let chain = build_controllers("multi", &cfg, &creds).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].name(), "command");
        assert_eq!(chain[1].name(), "command");
    }

    #[test]
    fn build_propagates_command_timeout() {
        let mut cfg = command_cfg();
        cfg.command_timeout = Duration::from_secs(42);
        let creds = Credentials::default();
        let chain = build_controllers("with-timeout", &cfg, &creds).unwrap();
        // We can't observe the timeout directly through the trait, but the
        // controller built with a non-default timeout must at least not panic
        // and expose the configured mode set.
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].supported_modes(), cfg.modes.unwrap());
    }
}
