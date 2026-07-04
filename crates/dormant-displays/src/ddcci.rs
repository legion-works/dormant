//! `ddcci` display controller — blanks monitors via DDC/CI VCP commands.
//!
//! ## Design
//!
//! DDC/CI (i2c-dev) calls are synchronous and can take 50–200 ms per VCP op.
//! Every hardware touch is wrapped in [`tokio::task::spawn_blocking`] so the
//! async executor is never blocked. No ddc-hi `Display` handle is cached
//! across operations — each blank/wake/probe re-enumerates, finds the matching
//! display, performs the op, and drops the handle. Enumeration is ~100 ms,
//! which is acceptable at blank/wake frequency.
//!
//! The controller exposes two blank modes:
//!
//! - `BrightnessZero` — set VCP code 0x10 (brightness) to 0. Always
//!   available. This is the audio-preserving fallback for monitors.
//! - `PowerOff` — set VCP code 0xD6 (power) to 0x05 (off). Only available
//!   after `probe` confirms the display supports 0xD6.
//!
//! ## Testability
//!
//! All ddc-hi interaction lives behind the [`VcpOps`] trait with a
//! [`RealVcp`] implementation (`spawn_blocking` inside) and a [`FakeVcp`] for
//! unit tests. The [`DdcciController`] logic is fully unit-testable.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use ddc_hi::Ddc;
use dormant_core::error::{DormantError, E_DISPLAY_IO};
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

// ── VCP code constants ─────────────────────────────────────────────────────────

/// VCP code for brightness (continuous, 0–100).
const VCP_BRIGHTNESS: u8 = 0x10;

/// VCP code for power control (D6 — optional feature).
const VCP_POWER: u8 = 0xD6;

/// D6 value: display on.
const D6_ON: u16 = 0x01;

/// D6 value: display off.
const D6_OFF: u16 = 0x05;

// ── VcpOps trait ───────────────────────────────────────────────────────────────

/// Information about a detected display returned by [`VcpOps::list_displays`].
#[derive(Debug, Clone)]
pub struct VcpDisplayInfo {
    /// Human-readable identifier string (backend:id manufacturer `model_name`).
    pub ident_string: String,
}

/// Abstract DDC/CI operations — real or fake.
///
/// Every method is `Send + Sync` so the trait object can be shared across
/// async tasks. The real implementation wraps blocking ddc-hi calls in
/// [`tokio::task::spawn_blocking`].
///
/// Methods take `&self` — fake implementations use interior mutability
/// (via [`StdMutex`]) for script state and call logging.
pub trait VcpOps: Send + Sync {
    /// Enumerate all DDC/CI-capable displays.
    fn list_displays(&self) -> Vec<VcpDisplayInfo>;

    /// Get the current value of a VCP feature code.
    ///
    /// # Errors
    ///
    /// Returns an error string if the VCP read fails (I/O error, display
    /// disconnected, or unsupported feature code).
    fn get_vcp(&self, ident: &str, code: u8) -> Result<u16, String>;

    /// Set a VCP feature code to a value.
    ///
    /// # Errors
    ///
    /// Returns an error string if the VCP write fails (I/O error, display
    /// disconnected, or unsupported feature code).
    fn set_vcp(&self, ident: &str, code: u8, value: u16) -> Result<(), String>;
}

// ── RealVcp — wraps ddc-hi in spawn_blocking ───────────────────────────────────

/// Real DDC/CI operations backed by ddc-hi, with every call wrapped in
/// [`tokio::task::spawn_blocking`].
pub struct RealVcp;

impl RealVcp {
    /// Enumerate synchronously (called inside `spawn_blocking`).
    fn enumerate_displays() -> Vec<(String, ddc_hi::Display)> {
        ddc_hi::Display::enumerate()
            .into_iter()
            .map(|d| (d.info.to_string(), d))
            .collect()
    }

    /// Find a display by ident string from an enumerated list.
    fn find_display<'a>(
        ident: &str,
        displays: &'a mut [(String, ddc_hi::Display)],
    ) -> Result<&'a mut ddc_hi::Display, String> {
        displays
            .iter_mut()
            .find(|(id, _)| id == ident)
            .map(|(_, d)| d)
            .ok_or_else(|| format!("display '{ident}' not found during re-enumeration"))
    }
}

impl VcpOps for RealVcp {
    fn list_displays(&self) -> Vec<VcpDisplayInfo> {
        let displays = ddc_hi::Display::enumerate();
        displays
            .into_iter()
            .map(|d| VcpDisplayInfo {
                ident_string: d.info.to_string(),
            })
            .collect()
    }

    fn get_vcp(&self, ident: &str, code: u8) -> Result<u16, String> {
        let ident = ident.to_string();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::task::spawn_blocking(move || {
                    let mut displays = Self::enumerate_displays();
                    let display = Self::find_display(&ident, &mut displays)?;
                    let vcp = display
                        .handle
                        .get_vcp_feature(code)
                        .map_err(|e| format!("get_vcp(0x{code:02X}) failed: {e}"))?;
                    Ok::<u16, String>(vcp.value())
                })
                .await
                .map_err(|e| format!("spawn_blocking join error: {e}"))?
            })
        })
    }

    fn set_vcp(&self, ident: &str, code: u8, value: u16) -> Result<(), String> {
        let ident = ident.to_string();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::task::spawn_blocking(move || {
                    let mut displays = Self::enumerate_displays();
                    let display = Self::find_display(&ident, &mut displays)?;
                    display
                        .handle
                        .set_vcp_feature(code, value)
                        .map_err(|e| format!("set_vcp(0x{code:02X}, {value}) failed: {e}"))
                })
                .await
                .map_err(|e| format!("spawn_blocking join error: {e}"))?
            })
        })
    }
}

// ── FakeVcp — scripted operations for tests ────────────────────────────────────

/// A scripted [`VcpOps`] implementation for unit tests.
///
/// Each call records its arguments in a call log (accessible via
/// `take_call_log`) and returns values from a pre-configured script.
/// All mutable state is behind [`StdMutex`] so the trait's `&self` methods
/// can mutate script state and the call log.
#[derive(Debug)]
pub struct FakeVcp {
    displays: Vec<VcpDisplayInfo>,
    /// (ident, code) → Result<value, err>
    get_script: StdMutex<Vec<ScriptEntry>>,
    /// (ident, code, value) → Result<(), err>
    set_script: StdMutex<Vec<SetScriptEntry>>,
    call_log: StdMutex<Vec<String>>,
}

/// A single scripted `get_vcp` response.
type ScriptEntry = ((String, u8), Result<u16, String>);

/// A single scripted `set_vcp` response.
type SetScriptEntry = ((String, u8, u16), Result<(), String>);

impl FakeVcp {
    /// Create a new `FakeVcp` with the given displays.
    #[must_use]
    pub fn new(displays: Vec<VcpDisplayInfo>) -> Self {
        Self {
            displays,
            get_script: StdMutex::new(Vec::new()),
            set_script: StdMutex::new(Vec::new()),
            call_log: StdMutex::new(Vec::new()),
        }
    }

    /// Add a scripted `get_vcp` response.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_get(&self, ident: &str, code: u8, result: Result<u16, String>) {
        self.get_script
            .lock()
            .unwrap()
            .push(((ident.to_string(), code), result));
    }

    /// Add a scripted `set_vcp` response.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_set(&self, ident: &str, code: u8, value: u16, result: Result<(), String>) {
        self.set_script
            .lock()
            .unwrap()
            .push(((ident.to_string(), code, value), result));
    }

    /// Drain the call log (FIFO).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn take_call_log(&self) -> Vec<String> {
        let mut log = self.call_log.lock().unwrap();
        std::mem::take(&mut *log)
    }
}

impl VcpOps for FakeVcp {
    fn list_displays(&self) -> Vec<VcpDisplayInfo> {
        self.displays.clone()
    }

    fn get_vcp(&self, ident: &str, code: u8) -> Result<u16, String> {
        self.call_log
            .lock()
            .unwrap()
            .push(format!("get_vcp({ident}, 0x{code:02X})"));
        let mut script = self.get_script.lock().unwrap();
        let idx = script
            .iter()
            .position(|((id, c), _)| id == ident && *c == code);
        match idx {
            Some(i) => {
                let ((_, _), result) = script.remove(i);
                result
            }
            None => Err(format!(
                "FakeVcp: no scripted response for get_vcp({ident}, 0x{code:02X})"
            )),
        }
    }

    fn set_vcp(&self, ident: &str, code: u8, value: u16) -> Result<(), String> {
        self.call_log
            .lock()
            .unwrap()
            .push(format!("set_vcp({ident}, 0x{code:02X}, {value})"));
        let mut script = self.set_script.lock().unwrap();
        let idx = script
            .iter()
            .position(|((id, c, v), _)| id == ident && *c == code && *v == value);
        match idx {
            Some(i) => {
                let ((_, _, _), result) = script.remove(i);
                result
            }
            None => Err(format!(
                "FakeVcp: no scripted response for set_vcp({ident}, 0x{code:02X}, {value})"
            )),
        }
    }
}

// ── DdcciController ────────────────────────────────────────────────────────────

/// Internal mutable state discovered during [`probe`].
#[derive(Debug, Clone, Default)]
struct DdcState {
    /// The ident string of the matched display (set during probe).
    matched_ident: Option<String>,
    /// Whether VCP code 0xD6 (power) is supported.
    d6_supported: bool,
    /// Saved brightness value from the last blank(BrightnessZero) call.
    saved_brightness: Option<u16>,
}

/// Display controller that blanks/wakes monitors via DDC/CI VCP commands.
///
/// ## Blank modes
///
/// | Mode | VCP | Always available? |
/// |---|---|---|
/// | `BrightnessZero` | 0x10 → 0 | Yes |
/// | `PowerOff` | 0xD6 → 0x05 | Only after probe confirms D6 support |
///
/// ## Matching
///
/// If `matcher` is `None` and exactly one DDC/CI display is detected, it is
/// auto-selected. If `matcher` is `Some(pattern)`, the display whose
/// `ident_string` contains `pattern` as a substring is selected. Zero matches
/// or multiple matches (without a matcher) produce a probe error.
pub struct DdcciController {
    matcher: Option<String>,
    restore_brightness: u8,
    ops: Arc<dyn VcpOps>,
    state: StdMutex<DdcState>,
}

impl DdcciController {
    /// Build a new `DdcciController` with real DDC/CI hardware access.
    #[must_use]
    pub fn new(matcher: Option<String>, restore_brightness: u8) -> Self {
        Self {
            matcher,
            restore_brightness,
            ops: Arc::new(RealVcp),
            state: StdMutex::new(DdcState::default()),
        }
    }

    /// Build a `DdcciController` with a custom [`VcpOps`] implementation
    /// (used by tests to inject [`FakeVcp`]).
    #[must_use]
    pub fn with_ops(matcher: Option<String>, restore_brightness: u8, ops: Arc<dyn VcpOps>) -> Self {
        Self {
            matcher,
            restore_brightness,
            ops,
            state: StdMutex::new(DdcState::default()),
        }
    }

    /// Find the matching display from an enumerated list.
    ///
    /// Returns the `ident_string` of the matched display, or an error.
    fn find_match(
        matcher: Option<&String>,
        displays: &[VcpDisplayInfo],
    ) -> Result<String, DormantError> {
        match matcher {
            Some(pattern) => {
                let matched_displays: Vec<&VcpDisplayInfo> = displays
                    .iter()
                    .filter(|d| d.ident_string.contains(pattern.as_str()))
                    .collect();
                match matched_displays.len() {
                    0 => Err(DormantError::DisplayIo {
                        controller: "ddcci".into(),
                        detail: format!(
                            "no DDC/CI display matches pattern '{pattern}' \
                             (found {} display(s) total)",
                            displays.len()
                        ),
                    }),
                    1 => Ok(matched_displays[0].ident_string.clone()),
                    _ => Err(DormantError::DisplayIo {
                        controller: "ddcci".into(),
                        detail: format!(
                            "multiple DDC/CI displays match pattern '{pattern}'; \
                             use a more specific ddc_display pattern",
                        ),
                    }),
                }
            }
            None => match displays.len() {
                0 => Err(DormantError::DisplayIo {
                    controller: "ddcci".into(),
                    detail: "no DDC/CI displays detected".into(),
                }),
                1 => Ok(displays[0].ident_string.clone()),
                n => Err(DormantError::DisplayIo {
                    controller: "ddcci".into(),
                    detail: format!(
                        "{n} DDC/CI displays detected — set ddc_display to \
                         select one",
                    ),
                }),
            },
        }
    }
}

impl DdcciController {
    /// Literal controller name — grep-stable, matches the `ddcci` config type.
    const NAME: &'static str = "ddcci";
}

#[async_trait]
impl DisplayController for DdcciController {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        let state = self.state.lock().unwrap();
        let mut modes = vec![BlankMode::BrightnessZero];
        if state.d6_supported {
            modes.push(BlankMode::PowerOff);
        }
        modes
    }

    async fn probe(&mut self) -> Result<(), DormantError> {
        let displays = self.ops.list_displays();
        let matched = Self::find_match(self.matcher.as_ref(), &displays)?;

        // Test D6 power control support.
        let d6_ok = self.ops.get_vcp(&matched, VCP_POWER).is_ok();

        if d6_ok {
            tracing::info!(
                event = "ddcci_probe",
                display = %matched,
                "DDC/CI display supports VCP 0xD6 power control",
            );
        } else {
            tracing::info!(
                event = "ddcci_probe",
                display = %matched,
                "DDC/CI display does not support VCP 0xD6 power control \
                 (falling back to brightness-zero only)",
            );
        }

        let mut state = self.state.lock().unwrap();
        state.matched_ident = Some(matched);
        state.d6_supported = d6_ok;
        Ok(())
    }

    async fn is_available(&self) -> bool {
        let state = self.state.lock().unwrap();
        let ident = match &state.matched_ident {
            Some(id) => id.clone(),
            None => return false,
        };
        drop(state);

        let displays = self.ops.list_displays();
        displays.iter().any(|d| d.ident_string == ident)
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        let state = self.state.lock().unwrap();
        let ident = match &state.matched_ident {
            Some(id) => id.clone(),
            None => {
                return Err(CmdFailure {
                    controller: Self::NAME.to_string(),
                    error: format!("{E_DISPLAY_IO}: controller not probed"),
                });
            }
        };
        let d6_supported = state.d6_supported;
        drop(state);

        match mode {
            BlankMode::BrightnessZero => {
                // Save current brightness, then set to 0.
                let current = self
                    .ops
                    .get_vcp(&ident, VCP_BRIGHTNESS)
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to read brightness: {e}"),
                    })?;

                self.ops
                    .set_vcp(&ident, VCP_BRIGHTNESS, 0)
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to set brightness to 0: {e}"),
                    })?;

                let mut state = self.state.lock().unwrap();
                state.saved_brightness = Some(current);
                Ok(())
            }
            BlankMode::PowerOff => {
                if !d6_supported {
                    return Err(CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!(
                            "{E_DISPLAY_IO}: VCP 0xD6 power control not supported \
                             on this display",
                        ),
                    });
                }
                self.ops
                    .set_vcp(&ident, VCP_POWER, D6_OFF)
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to set power off: {e}"),
                    })?;
                Ok(())
            }
            BlankMode::ScreenOffAudioOn => Err(CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!("{E_DISPLAY_IO}: unsupported blank mode {mode:?}"),
            }),
        }
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let state = self.state.lock().unwrap();
        let ident = match &state.matched_ident {
            Some(id) => id.clone(),
            None => {
                return Err(CmdFailure {
                    controller: Self::NAME.to_string(),
                    error: format!("{E_DISPLAY_IO}: controller not probed"),
                });
            }
        };
        let d6_supported = state.d6_supported;
        let saved = state.saved_brightness;
        let restore = saved.unwrap_or(u16::from(self.restore_brightness));
        drop(state);

        // If D6 is supported, try to power on first (ignore error — the
        // brightness restore is the primary wake mechanism).
        if d6_supported {
            let _ = self.ops.set_vcp(&ident, VCP_POWER, D6_ON);
        }

        // Restore brightness.
        self.ops
            .set_vcp(&ident, VCP_BRIGHTNESS, restore)
            .map_err(|e| CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!("{E_DISPLAY_IO}: failed to restore brightness: {e}"),
            })?;

        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    /// Helper: build a `FakeVcp` with one display.
    fn single_display_vcp() -> FakeVcp {
        FakeVcp::new(vec![VcpDisplayInfo {
            ident_string: "i2c-dev:56 DEL DELL U2723QE".into(),
        }])
    }

    /// Helper: build a `FakeVcp` with two displays.
    fn two_display_vcp() -> FakeVcp {
        FakeVcp::new(vec![
            VcpDisplayInfo {
                ident_string: "i2c-dev:56 DEL DELL U2723QE".into(),
            },
            VcpDisplayInfo {
                ident_string: "i2c-dev:57 SAM SAMSUNG".into(),
            },
        ])
    }

    // ── probe tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn probe_single_display_auto_match() {
        let fake = Arc::new(single_display_vcp());
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
        ctrl.probe().await.unwrap();

        let state = ctrl.state.lock().unwrap();
        assert_eq!(
            state.matched_ident.as_deref(),
            Some("i2c-dev:56 DEL DELL U2723QE")
        );
    }

    #[tokio::test]
    async fn probe_matcher_substring() {
        let fake = Arc::new(two_display_vcp());
        let mut ctrl = DdcciController::with_ops(
            Some("DELL".into()),
            80,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        let state = ctrl.state.lock().unwrap();
        assert_eq!(
            state.matched_ident.as_deref(),
            Some("i2c-dev:56 DEL DELL U2723QE")
        );
    }

    #[tokio::test]
    async fn probe_no_match_errs() {
        let fake = Arc::new(single_display_vcp());
        let mut ctrl = DdcciController::with_ops(
            Some("NONEXISTENT".into()),
            80,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        let err = ctrl.probe().await.unwrap_err();
        assert!(
            err.to_string().contains("no DDC/CI display matches"),
            "error should mention no match: {err}"
        );
    }

    #[tokio::test]
    async fn probe_multiple_without_matcher_errs() {
        let fake = Arc::new(two_display_vcp());
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
        let err = ctrl.probe().await.unwrap_err();
        assert!(
            err.to_string().contains("set ddc_display"),
            "error should tell user to set ddc_display: {err}"
        );
    }

    #[tokio::test]
    async fn probe_detects_d6() {
        // D6 supported
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
        ctrl.probe().await.unwrap();
        {
            let state = ctrl.state.lock().unwrap();
            assert!(state.d6_supported, "D6 should be supported");
        }
        assert!(
            ctrl.supported_modes().contains(&BlankMode::PowerOff),
            "PowerOff should be in supported_modes when D6 is supported"
        );

        // D6 not supported
        let fake2 = Arc::new({
            let f = single_display_vcp();
            f.expect_get(
                "i2c-dev:56 DEL DELL U2723QE",
                VCP_POWER,
                Err("unsupported".into()),
            );
            f
        });
        let mut ctrl2 = DdcciController::with_ops(None, 80, Arc::clone(&fake2) as Arc<dyn VcpOps>);
        ctrl2.probe().await.unwrap();
        {
            let state = ctrl2.state.lock().unwrap();
            assert!(!state.d6_supported, "D6 should NOT be supported");
        }
        assert!(
            !ctrl2.supported_modes().contains(&BlankMode::PowerOff),
            "PowerOff should NOT be in supported_modes when D6 is unsupported"
        );
    }

    // ── blank tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn blank_brightness_saves_then_zeroes() {
        let fake = Arc::new({
            let f = single_display_vcp();
            // probe: D6 not supported
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
        ctrl.probe().await.unwrap();

        // Now blank with BrightnessZero — needs get(0x10) then set(0x10, 0)
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(75));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));

        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();

        let state = ctrl.state.lock().unwrap();
        assert_eq!(state.saved_brightness, Some(75));

        let log = fake.take_call_log();
        assert!(
            log.iter()
                .any(|l| l.contains("get_vcp") && l.contains("0x10")),
            "should have read brightness before setting: {log:?}"
        );
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains('0')),
            "should have set brightness to 0: {log:?}"
        );
    }

    #[tokio::test]
    async fn wake_restores_saved_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
        ctrl.probe().await.unwrap();

        // Blank to save brightness
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(42));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();

        // Wake — should restore saved 42
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 42, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains("42")),
            "should restore saved brightness 42: {log:?}"
        );
    }

    #[tokio::test]
    async fn wake_without_saved_uses_config_default() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 55, Arc::clone(&fake) as Arc<dyn VcpOps>);
        ctrl.probe().await.unwrap();

        // Wake without any blank first — should use restore_brightness (55)
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 55, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains("55")),
            "should restore config default 55: {log:?}"
        );
    }

    #[tokio::test]
    async fn blank_power_off_requires_d6() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
        ctrl.probe().await.unwrap();

        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(
            err.error.contains("not supported"),
            "error should mention D6 not supported: {err}"
        );
    }

    #[tokio::test]
    async fn wake_tries_d6_on_then_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
        ctrl.probe().await.unwrap();

        // Blank with PowerOff
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_OFF, Ok(()));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();

        // Wake — D6_ON first (ignored if err), then brightness restore
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_ON, Ok(()));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 80, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        let d6_idx = log
            .iter()
            .position(|l| l.contains("set_vcp") && l.contains("0xD6") && l.contains('1'));
        let br_idx = log
            .iter()
            .position(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains("80"));
        assert!(
            d6_idx.is_some() && br_idx.is_some(),
            "wake should call both D6_ON and brightness restore: {log:?}"
        );
        assert!(
            d6_idx < br_idx,
            "D6_ON should come before brightness restore: {log:?}"
        );
    }
}
