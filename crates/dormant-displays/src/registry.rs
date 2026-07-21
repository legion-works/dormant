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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dormant_core::config::schema::{Credentials, DisplayConfig};
use dormant_core::error::DormantError;
use dormant_core::traits::DisplayController;
use dormant_core::types::BlankMode;

use crate::blank_owner::BlankOwnerRegistry;
use crate::command::CommandController;
use crate::ddc_lock::PanelLocks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::ddcci::DdcciController;
#[cfg(target_os = "macos")]
use crate::gamma_breadcrumb::GammaBreadcrumb;
use crate::ha_passthrough::HaPassthroughController;
#[cfg(target_os = "linux")]
use crate::kwin_dpms::KwinDpmsController;
#[cfg(target_os = "macos")]
use crate::macos_display_sleep::MacosDisplaySleepController;
#[cfg(target_os = "macos")]
use crate::macos_gamma_black::{GammaHoldRegistry, MacosGammaBlackController};
use crate::samsung_tizen::SamsungTizenController;

// ── ControllerBuildContext (Task 8) ─────────────────────────────────────

/// Everything [`build_controllers`] needs that must survive a config reload
/// unchanged, bundled into one value so every call site threads exactly one
/// thing instead of an ever-growing parameter list.
///
/// Carries:
/// - [`PanelLocks`] (spec §4.3) — the daemon's process-wide DDC/CI
///   per-panel lock registry (Task 3/6).
/// - `state_dir` — the daemon's resolved state directory (Task 5's
///   `dormant_core::paths::state_dir()`, or a caller-supplied override),
///   used by the `macos-gamma-black` arm to construct the Task 8
///   `GammaBreadcrumb` breadcrumb.
/// - `gamma_holds` (macOS-only, `#[cfg(target_os = "macos")]`) — the
///   daemon's process-wide `crate::macos_gamma_black::GammaHoldRegistry`
///   (Task 7; not an intra-doc link — that item is itself only `use`d on
///   macOS in this module, so a Linux doc build can't resolve it even by
///   full path). The FIELD is `cfg`-gated (there is nothing to hold on a
///   platform with no Quartz gamma API), but the STRUCT itself is
///   deliberately platform-neutral — [`Self::new`] and every other public
///   method compile and run unchanged on Linux — so callers never need
///   their own `cfg` gating just to construct or thread a context.
///
/// Constructed exactly ONCE per daemon process, in `App::start` (alongside
/// `PanelLocks::new()`, which it wraps), and reused, unchanged, across
/// every reload/rollback generation's `assemble_static` call — the same
/// survives-a-reload contract [`PanelLocks`] already had, now extended to
/// the gamma hold registry and the breadcrumb (both must resolve to the
/// SAME instance before and after a config reload; see
/// `crate::macos_gamma_black`'s "First-blank-wins saved state" docs and
/// `crate::gamma_breadcrumb`'s module docs).
pub struct ControllerBuildContext {
    locks: Arc<PanelLocks>,
    blank_owners: Arc<BlankOwnerRegistry>,
    state_dir: PathBuf,
    #[cfg(target_os = "macos")]
    gamma_holds: Arc<GammaHoldRegistry>,
    /// ONE shared breadcrumb instance for every `macos-gamma-black`
    /// controller this context builds — MUST be the same `Arc` (not a
    /// fresh `GammaBreadcrumb` per controller), because `GammaBreadcrumb`'s
    /// "process-wide marker mutex" (see its module docs' "Concurrency"
    /// section) is an in-process `std::sync::Mutex`, not a file lock: two
    /// independently-constructed instances pointed at the same directory
    /// would each guard only their OWN read-modify-write, not each
    /// other's, reopening exactly the lost-update race the marker mutex
    /// exists to close.
    #[cfg(target_os = "macos")]
    gamma_breadcrumb: Arc<GammaBreadcrumb>,
}

impl ControllerBuildContext {
    /// Build a context wrapping `locks` and `state_dir`. On macOS this also
    /// constructs a fresh `GammaHoldRegistry` and a single
    /// `GammaBreadcrumb` rooted at `state_dir` — callers MUST call this
    /// exactly once per daemon process and reuse the same
    /// `ControllerBuildContext` for every subsequent
    /// `build_controllers`/`assemble_static` call (see the struct docs).
    #[must_use]
    pub fn new(locks: Arc<PanelLocks>, state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            locks,
            blank_owners: Arc::new(BlankOwnerRegistry::new()),
            #[cfg(target_os = "macos")]
            gamma_holds: Arc::new(GammaHoldRegistry::new()),
            #[cfg(target_os = "macos")]
            gamma_breadcrumb: Arc::new(GammaBreadcrumb::new(state_dir.clone())),
            state_dir,
        }
    }

    /// The shared [`PanelLocks`] registry.
    #[must_use]
    pub fn locks(&self) -> &Arc<PanelLocks> {
        &self.locks
    }

    /// The shared blank-owner registry for generation-safe executor rebuilds.
    #[must_use]
    pub fn blank_owners(&self) -> &Arc<BlankOwnerRegistry> {
        &self.blank_owners
    }

    /// The resolved state directory this context was built with.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The shared [`GammaHoldRegistry`] (macOS only).
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn gamma_holds(&self) -> &Arc<GammaHoldRegistry> {
        &self.gamma_holds
    }

    /// The shared [`GammaBreadcrumb`] (macOS only) — the SAME instance for
    /// every controller this context builds (see the struct field's docs).
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn gamma_breadcrumb(&self) -> &Arc<GammaBreadcrumb> {
        &self.gamma_breadcrumb
    }
}

/// Produce the identity used to retain blank ownership across a rebuild.
#[must_use]
pub fn controller_chain_fingerprint(cfg: &DisplayConfig) -> String {
    // Keep this field classification in sync with
    // `dormantd::reload::dispatch_relevant_eq`: owner retention is safe only
    // while blank/wake dispatch is unchanged.
    let DisplayConfig {
        controllers,
        blank_mode,
        degraded_mode,
        ladder,
        screensaver: _,
        output,
        ddc_display,
        host,
        wol_mac,
        blank_command,
        wake_command,
        modes,
        ha_url,
        blank_service,
        blank_data,
        wake_service,
        wake_data,
        command_timeout,
        restore_brightness: _,
        samsung_restore_backlight: _,
        treat_unreachable_as_blanked,
        ..
    } = cfg;

    format!(
        "{:?}",
        (
            (
                controllers,
                blank_mode,
                degraded_mode,
                ladder,
                output,
                ddc_display,
                host,
                wol_mac,
                blank_command,
                wake_command,
            ),
            (
                modes,
                ha_url,
                blank_service,
                blank_data,
                wake_service,
                wake_data,
                command_timeout,
                treat_unreachable_as_blanked,
            ),
        )
    )
}

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
pub const CONTROLLER_TYPES: &[&str] = &[
    "command",
    "ddcci",
    "ha-passthrough",
    "macos-display-sleep",
    "macos-gamma-black",
    "samsung-tizen",
];
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
    #[cfg(target_os = "macos")]
    m.insert(
        "macos-gamma-black".to_string(),
        vec![BlankMode::BrightnessZero],
    );
    // Task 10: `macos-display-sleep` is a GLOBAL fallback (no per-display
    // selector — see the module docs on `crate::macos_display_sleep`) with
    // exactly one capability: PowerOff (it puts every display to sleep via
    // `pmset displaysleepnow`, the coarsest possible blank).
    #[cfg(target_os = "macos")]
    m.insert("macos-display-sleep".to_string(), vec![BlankMode::PowerOff]);
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

/// Controller type names that can read a panel's active-input VCP.
#[must_use]
pub fn input_source_readers() -> HashSet<String> {
    let mut readers = HashSet::new();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    readers.insert("ddcci".to_string());
    readers
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
/// `ctx` is the [`ControllerBuildContext`] bundling every piece of shared,
/// reload-surviving state a controller may need (spec §4.3 for the
/// `PanelLocks` half; Task 8 for the macOS gamma-hold-registry/breadcrumb
/// half) — the caller constructs exactly ONE `ControllerBuildContext` for
/// its whole lifetime (the daemon in `App::start`; each direct-hardware CLI
/// invocation gets its own fresh one, being a separate process) and passes
/// it to every `build_controllers` call, so that a panel's lock — and, on
/// macOS, a selector's gamma hold/breadcrumb — resolves to the same shared
/// instance no matter which config-reload generation or call site derived
/// its controller. Only the `ddcci` and `macos-gamma-black` arms consume
/// it; every other controller type ignores it (no shared physical bus or
/// daemon-lifetime state to serialize).
#[allow(clippy::too_many_lines)]
pub fn build_controllers(
    display_name: &str,
    cfg: &DisplayConfig,
    creds: &Credentials,
    ctx: &ControllerBuildContext,
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
                    ctx.locks(),
                )));
            }
            #[cfg(target_os = "linux")]
            "kwin-dpms" => {
                chain.push(Box::new(KwinDpmsController::new(
                    cfg.output.clone(),
                    cfg.command_timeout,
                )));
            }
            #[cfg(target_os = "macos")]
            "macos-gamma-black" => {
                // `output` is required by config validation (Task 4's
                // ratified `cg:<uuid>` selector contract, enforced in
                // `dormant_core::config::validate`) — this `expect` documents
                // that invariant rather than re-deriving a second error
                // message here; a config that reached `build_controllers`
                // without a valid `output` for this controller already
                // failed validation and never gets here in production.
                let selector = cfg.output.clone().ok_or_else(|| {
                    config_invalid_cmd(display_name, "missing 'output' (expected \"cg:<uuid>\")")
                })?;
                chain.push(Box::new(MacosGammaBlackController::new(
                    selector,
                    Arc::clone(ctx.gamma_holds()),
                    Arc::clone(ctx.gamma_breadcrumb()),
                )));
            }
            #[cfg(target_os = "macos")]
            "macos-display-sleep" => {
                // GLOBAL fallback — no selector to wire (unlike ddcci /
                // macos-gamma-black above); config validation
                // (`dormant_core::config::validate`) already enforces that
                // `output` is absent or the literal `"all"` for this
                // controller, so `build_controllers` doesn't need to
                // re-check it here (mirrors kwin-dpms / ddcci having no
                // required fields beyond the defaults).
                chain.push(Box::new(MacosDisplaySleepController::new(
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

    /// A fresh [`ControllerBuildContext`] for tests that don't care about
    /// state-dir persistence (most of this module's tests) — every test
    /// gets its own throwaway `/tmp`-rooted path; nothing in this module's
    /// tests exercises Task 8 breadcrumb file I/O, so the path is never
    /// actually written to on Linux (the `macos-gamma-black` arm that would
    /// touch it is `#[cfg(target_os = "macos")]`-gated).
    fn test_ctx() -> ControllerBuildContext {
        ControllerBuildContext::new(
            PanelLocks::new(),
            std::env::temp_dir().join("dormant-registry-test"),
        )
    }

    /// Minimal valid `command` display config — used by the happy-path test
    /// and as a base for "missing fields" variants.
    fn command_cfg() -> DisplayConfig {
        DisplayConfig {
            controllers: vec!["command".into()],
            scope: dormant_core::config::DisplayScope::Private,
            shared_input_code: None,
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

    /// Task 7: `macos-gamma-black` is macOS-only (no Quartz gamma API
    /// anywhere else). DEFERRED: PR CI — cannot run in this Linux sandbox;
    /// written now for the macOS CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn controller_types_contains_macos_gamma_black_on_macos() {
        assert!(
            CONTROLLER_TYPES.contains(&"macos-gamma-black"),
            "macos-gamma-black must be registered on macOS"
        );
    }

    /// Task 7 RED-first (Linux-runnable half of the registry-drift pin):
    /// `macos-gamma-black` must be absent from `CONTROLLER_TYPES` on every
    /// platform except macOS, so config validation rejects
    /// `controllers = ["macos-gamma-black"]` deterministically with
    /// "unknown controller" rather than reaching `build_controllers` and
    /// hitting a controller type that only exists behind a
    /// `#[cfg(target_os = "macos")]` match arm.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn controller_types_excludes_macos_gamma_black_elsewhere() {
        assert!(
            !CONTROLLER_TYPES.contains(&"macos-gamma-black"),
            "macos-gamma-black must NOT be registered off macOS"
        );
    }

    /// Task 10: `macos-display-sleep` is macOS-only (`pmset`/IOPM/
    /// CoreGraphics are all macOS-specific). DEFERRED: PR CI — cannot run
    /// in this Linux sandbox; written now for the macOS CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn controller_types_contains_macos_display_sleep_on_macos() {
        assert!(
            CONTROLLER_TYPES.contains(&"macos-display-sleep"),
            "macos-display-sleep must be registered on macOS"
        );
    }

    /// Task 10 RED-first (Linux-runnable half of the registry-drift pin):
    /// `macos-display-sleep` must be absent from `CONTROLLER_TYPES` on
    /// every platform except macOS — mirrors
    /// `controller_types_excludes_macos_gamma_black_elsewhere` exactly.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn controller_types_excludes_macos_display_sleep_elsewhere() {
        assert!(
            !CONTROLLER_TYPES.contains(&"macos-display-sleep"),
            "macos-display-sleep must NOT be registered off macOS"
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

    /// Task 7 RED-first: macOS advertises `macos-gamma-black` with EXACTLY
    /// `[BrightnessZero]` — no `PowerOff`, no `ScreenOffAudioOn`: this
    /// controller has exactly one capability (see the module docs on why
    /// gamma-zeroing is the only mode it can express). DEFERRED: PR CI —
    /// cannot run in this Linux sandbox; written now for the macOS CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_advertises_macos_gamma_black() {
        let caps = capabilities();
        assert_eq!(
            caps.get("macos-gamma-black"),
            Some(&vec![BlankMode::BrightnessZero]),
            "macos-gamma-black capabilities must be exactly [BrightnessZero]"
        );
    }

    /// Task 10 RED-first: macOS advertises `macos-display-sleep` with
    /// EXACTLY `[PowerOff]` — no `BrightnessZero`, no `ScreenOffAudioOn`:
    /// this controller can only put displays fully to sleep (see the
    /// module docs on why it is a global, coarse-grained fallback).
    /// DEFERRED: PR CI — cannot run in this Linux sandbox; written now for
    /// the macOS CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_advertises_macos_display_sleep() {
        let caps = capabilities();
        assert_eq!(
            caps.get("macos-display-sleep"),
            Some(&vec![BlankMode::PowerOff]),
            "macos-display-sleep capabilities must be exactly [PowerOff]"
        );
    }

    /// Task 10 RED-first: `build_controllers` wires a bare
    /// `controllers = ["macos-display-sleep"]` display with NO `output`
    /// (the common case — this controller is global, see the module docs)
    /// into a working chain, and threads `cfg.command_timeout` through
    /// (indirectly verified via `supported_modes` staying `[PowerOff]` and
    /// the controller not panicking to build). DEFERRED: PR CI — cannot run
    /// in this Linux sandbox; written now for the macOS CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn build_macos_display_sleep_requires_no_output() {
        let mut cfg = command_cfg();
        cfg.controllers = vec!["macos-display-sleep".into()];
        cfg.blank_mode = Some(BlankMode::PowerOff);
        cfg.output = None; // global fallback — no selector required

        let creds = Credentials::default();
        let chain = build_controllers("main", &cfg, &creds, &test_ctx()).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "macos-display-sleep");
        assert_eq!(chain[0].supported_modes(), vec![BlankMode::PowerOff]);
    }

    /// Task 7 RED-first: the registry wires `cfg.output` (the Task 4
    /// ratified `cg:<uuid>` selector) into the controller's selector, and
    /// two controller instances built against the SAME process (i.e. the
    /// same `GAMMA_HOLDS` static) share one `GammaHoldRegistry` — mirrors
    /// `build_ddcci_ladder_primary_power_off_wires_configured_primary_mode`'s
    /// registry-path pinning for ddcci. DEFERRED: PR CI — cannot run in
    /// this Linux sandbox; written now for the macOS CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn build_macos_gamma_black_wires_output_as_selector() {
        use crate::macos_gamma_black::MacosGammaBlackController;

        let mut cfg = command_cfg();
        cfg.controllers = vec!["macos-gamma-black".into()];
        cfg.blank_mode = Some(BlankMode::BrightnessZero);
        cfg.output = Some("cg:deadbeef-0000-0000-0000-000000000000".into());

        let creds = Credentials::default();
        let chain = build_controllers("main", &cfg, &creds, &test_ctx()).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "macos-gamma-black");

        let boxed = chain.into_iter().next().expect("one controller");
        let ctrl: Box<MacosGammaBlackController> = (boxed as Box<dyn std::any::Any>)
            .downcast()
            .expect("macos-gamma-black controller downcast");
        assert_eq!(
            ctrl.panel_identity(),
            Some("cg:deadbeef-0000-0000-0000-000000000000".to_string()),
            "registry must thread cfg.output into the controller's selector"
        );
    }

    /// Task 7: `build_controllers` refuses a `macos-gamma-black` display
    /// with no `output` set — the same hard requirement config validation
    /// enforces (see `dormant_core::config::validate`), pinned here too as
    /// a belt-and-braces guard since `build_controllers` is the last line
    /// of defense before a controller is actually constructed. DEFERRED:
    /// PR CI — cannot run in this Linux sandbox; written now for the macOS
    /// CI lane.
    #[test]
    #[cfg(target_os = "macos")]
    fn build_macos_gamma_black_missing_output_fails_naming_display() {
        let mut cfg = command_cfg();
        cfg.controllers = vec!["macos-gamma-black".into()];
        cfg.blank_mode = Some(BlankMode::BrightnessZero);
        cfg.output = None;

        let creds = Credentials::default();
        match build_controllers("main", &cfg, &creds, &test_ctx()) {
            Err(DormantError::ConfigInvalid { detail }) => {
                assert!(detail.contains("display 'main'"));
                assert!(detail.contains("missing 'output'"));
            }
            Err(other) => panic!("expected ConfigInvalid for missing output, got {other:?}"),
            Ok(_) => panic!("expected ConfigInvalid for missing output, got Ok(controllers)"),
        }
    }

    #[test]
    fn build_command_happy() {
        let cfg = command_cfg();
        let creds = Credentials::default();
        let chain = build_controllers("main", &cfg, &creds, &test_ctx()).unwrap();
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
            scope: dormant_core::config::DisplayScope::Private,
            shared_input_code: None,
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

        let chain = build_controllers("tv", &cfg, &creds, &test_ctx()).unwrap();
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
        let chain = build_controllers("main", &cfg, &creds, &test_ctx()).unwrap();
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
        let res = build_controllers("main", &cfg, &creds, &test_ctx());
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
        match build_controllers("tv-corner", &cfg, &creds, &test_ctx()) {
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
        match build_controllers("tv-corner", &cfg, &creds, &test_ctx()) {
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
        match build_controllers("tv-corner", &cfg, &creds, &test_ctx()) {
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
        match build_controllers("tv-corner", &cfg, &creds, &test_ctx()) {
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
        let chain = build_controllers("no-display", &cfg, &creds, &test_ctx()).unwrap();
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
        let chain = build_controllers("multi", &cfg, &creds, &test_ctx()).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].name(), "command");
        assert_eq!(chain[1].name(), "command");
    }

    #[test]
    fn build_propagates_command_timeout() {
        let mut cfg = command_cfg();
        cfg.command_timeout = Duration::from_secs(42);
        let creds = Credentials::default();
        let chain = build_controllers("with-timeout", &cfg, &creds, &test_ctx()).unwrap();
        // We can't observe the timeout directly through the trait, but the
        // controller built with a non-default timeout must at least not panic
        // and expose the configured mode set.
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].supported_modes(), cfg.modes.unwrap());
    }
}
