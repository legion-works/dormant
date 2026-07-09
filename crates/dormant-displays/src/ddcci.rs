//! `ddcci` display controller — blanks monitors via DDC/CI VCP commands.
//!
//! The controller exposes two blank modes:
//!
//! - `BrightnessZero` — set VCP code 0x10 (brightness) to 0. Always
//!   available. This is the audio-preserving fallback for monitors.
//! - `PowerOff` — set VCP code 0xD6 (power) to 0x05 (off). Only available
//!   after `probe` confirms the display supports 0xD6.
//!
//! ## Matching
//!
//! The `ddc_display` config key is a **case-sensitive substring** match against
//! the display's identifier string (e.g. `"DELL"` matches
//! `"i2c-dev:56 DEL DELL U2723QE"`). An empty or absent matcher auto-selects
//! the single detected display; zero or multiple matches produce a probe error.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use dormant_core::error::{DormantError, E_DISPLAY_IO};
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

#[cfg(target_os = "linux")]
use crate::vcp_ops::RealVcp;
use crate::vcp_ops::VcpOps;

// ── VCP code constants ─────────────────────────────────────────────────────────

/// VCP code for brightness (continuous, 0–100).
const VCP_BRIGHTNESS: u8 = 0x10;

/// VCP code for power control (D6 — optional feature).
const VCP_POWER: u8 = 0xD6;

/// D6 value: display on.
const D6_ON: u16 = 0x01;

/// D6 value: display off.
const D6_OFF: u16 = 0x05;

// ── DdcciController ────────────────────────────────────────────────────────────

/// Internal mutable state discovered during [`probe`].
#[derive(Debug, Default)]
struct DdcState {
    /// The ident string of the matched display (set during probe).
    matched_ident: Option<String>,
    /// Whether VCP code 0xD6 (power) is supported.
    d6_supported: bool,
    /// Saved brightness value from the first `blank(BrightnessZero)` call.
    /// `None` means no blank has happened yet (wake uses config default).
    saved_brightness: Option<u16>,
    /// The actual blank mode of the last successful `blank()` — read by
    /// `wake()` to decide whether to restore brightness. Recorded AFTER
    /// success so a failed blank doesn't poison the wake path.
    ///
    /// `None` means no blank has happened yet (daemon just started /
    /// reloaded); wake falls through to [`DdcciController::configured_primary_mode`].
    last_blank_mode: Option<BlankMode>,
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
/// `ident_string` contains `pattern` as a **case-sensitive substring** is
/// selected. Zero matches or multiple matches (without a matcher) produce a
/// probe error.
pub struct DdcciController {
    matcher: Option<String>,
    restore_brightness: u8,
    /// Configured primary blank mode — sourced from
    /// [`dormant_core::config::schema::DisplayConfig::primary_blank_mode`].
    /// Used by `wake()` as the fallback when `last_blank_mode` is `None`
    /// (daemon restart / reload). A `PowerOff`-primary display that wakes
    /// before any blank has run must still hit the D6-on path, not the
    /// brightness-restore path.
    configured_primary_mode: BlankMode,
    ops: Arc<dyn VcpOps>,
    state: StdMutex<DdcState>,
}

impl DdcciController {
    /// Build a new `DdcciController` with real DDC/CI hardware access.
    ///
    /// Only available on Linux — DDC/CI requires platform I²C support.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn new(
        matcher: Option<String>,
        restore_brightness: u8,
        configured_primary_mode: BlankMode,
    ) -> Self {
        Self {
            matcher,
            restore_brightness,
            configured_primary_mode,
            ops: Arc::new(RealVcp),
            state: StdMutex::new(DdcState::default()),
        }
    }

    /// Build a `DdcciController` with a custom `VcpOps` implementation
    /// (used by tests to inject a fake).
    #[must_use]
    pub fn with_ops(
        matcher: Option<String>,
        restore_brightness: u8,
        configured_primary_mode: BlankMode,
        ops: Arc<dyn VcpOps>,
    ) -> Self {
        Self {
            matcher,
            restore_brightness,
            configured_primary_mode,
            ops,
            state: StdMutex::new(DdcState::default()),
        }
    }

    /// Test-only accessor: read the configured primary mode the registry
    /// wired in. Used by the registry-path test that asserts end-to-end
    /// config → controller wiring (mirrors `SamsungTizenController::configured_primary_mode`).
    #[cfg(test)]
    pub(crate) fn configured_primary_mode(&self) -> BlankMode {
        self.configured_primary_mode
    }

    /// Find the matching display from an enumerated list.
    ///
    /// The match is a **case-sensitive substring** check against each display's
    /// `ident_string`. Returns the `ident_string` of the matched display, or an
    /// error.
    fn find_match(
        matcher: Option<&String>,
        displays: &[crate::vcp_ops::VcpDisplayInfo],
    ) -> Result<String, DormantError> {
        match matcher {
            Some(pattern) => {
                let matched_displays: Vec<&crate::vcp_ops::VcpDisplayInfo> = displays
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
        let displays = self.ops.list_displays().await;
        let matched = Self::find_match(self.matcher.as_ref(), &displays)?;

        // Test D6 power control support.
        let d6_ok = self.ops.get_vcp(&matched, VCP_POWER).await.is_ok();

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
        let ident = {
            let state = self.state.lock().unwrap();
            match &state.matched_ident {
                Some(id) => id.clone(),
                None => return false,
            }
        };

        let displays = self.ops.list_displays().await;
        displays.iter().any(|d| d.ident_string == ident)
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        let (ident, d6_supported) = {
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
            (ident, state.d6_supported)
        };

        let result = match mode {
            BlankMode::BrightnessZero => {
                // Save current brightness, then set to 0.
                let current = self
                    .ops
                    .get_vcp(&ident, VCP_BRIGHTNESS)
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to read brightness: {e}"),
                    })?;

                self.ops
                    .set_vcp(&ident, VCP_BRIGHTNESS, 0)
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to set brightness to 0: {e}"),
                    })?;

                // Only save on the FIRST blank — a second blank while already
                // blanked reads current=0 and would clobber the real saved
                // value, causing wake to restore 0 (stuck dark).
                let mut state = self.state.lock().unwrap();
                if state.saved_brightness.is_none() {
                    state.saved_brightness = Some(current);
                }
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
                    .await
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
        };

        // Record the actual blank that succeeded — `wake()` reads this to
        // decide whether to write brightness. Recording AFTER success means
        // a failed blank doesn't poison the wake path (mirrors the
        // samsung-tizen pattern).
        if result.is_ok() {
            let mut state = self.state.lock().unwrap();
            state.last_blank_mode = Some(mode);
        }
        result
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let (ident, d6_supported, effective_mode) = {
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
            // Reverse whatever the LAST successful blank actually did.
            // `last_blank_mode.or(Some(configured_primary_mode))` means a
            // fresh daemon (no blank yet) still takes the configured
            // primary path; once any blank runs, that one is authoritative.
            let mode = state
                .last_blank_mode
                .unwrap_or(self.configured_primary_mode);
            (ident, state.d6_supported, mode)
        };

        match effective_mode {
            BlankMode::PowerOff => {
                // A `PowerOff` blank never touched brightness — the panel
                // retained it across the power cycle. Wake does D6-on ONLY
                // and does NOT write brightness, so a user who tuned
                // brightness to e.g. 100 keeps 100 after every wake. (Live-
                // confirmed: this was clobbering brightness to the
                // config default — the operator's monitor resets to the
                // `restore_brightness` value on every presence-driven
                // wake.)
                if d6_supported {
                    self.ops
                        .set_vcp(&ident, VCP_POWER, D6_ON)
                        .await
                        .map_err(|e| CmdFailure {
                            controller: Self::NAME.to_string(),
                            error: format!("{E_DISPLAY_IO}: failed to set power on: {e}"),
                        })?;
                }
                Ok(())
            }
            BlankMode::BrightnessZero => {
                // Brightness restore + the restart-safety-net fallback to
                // `restore_brightness` (operator-tuned config default)
                // when the saved value was lost across a restart.
                let restore = {
                    let state = self.state.lock().unwrap();
                    state
                        .saved_brightness
                        .unwrap_or(u16::from(self.restore_brightness))
                };

                // If D6 is supported, try to power on first (ignore error
                // — the brightness restore is the primary wake mechanism
                // for a brightness-zero blanked display).
                if d6_supported {
                    let _ = self.ops.set_vcp(&ident, VCP_POWER, D6_ON).await;
                }

                // Restore brightness.
                self.ops
                    .set_vcp(&ident, VCP_BRIGHTNESS, restore)
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to restore brightness: {e}"),
                    })?;

                // Clear saved_brightness so the NEXT blank cycle re-saves
                // fresh. Without this, a user who manually raises
                // brightness between cycles gets a stale restore.
                let mut state = self.state.lock().unwrap();
                state.saved_brightness = None;

                Ok(())
            }
            BlankMode::ScreenOffAudioOn => Err(CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!(
                    "{E_DISPLAY_IO}: wake does not support blank mode {effective_mode:?} \
                     for this controller"
                ),
            }),
        }
    }

    /// Read the panel state — brightness (VCP `0x10`, 0–100) and power
    /// (VCP `0xD6`, `0x01` → On, anything else → Standby).
    ///
    /// Returns `None` when the controller has not been probed (no matched
    /// ident) — the exercise handler treats `None` as `Unconfirmable`
    /// rather than guessing at a state.  VCP read errors propagate as
    /// `None` (the same honest answer) so the exercise surfaces a
    /// readback failure rather than fabricating a state.
    async fn read_state(&self) -> Option<dormant_core::traits::PanelState> {
        use dormant_core::traits::{PanelState, PowerState};

        let ident = {
            let state = self.state.lock().unwrap();
            state.matched_ident.clone()
        }?;

        // Brightness: 0x10 is continuous 0–100 on every DDC/CI monitor.
        let brightness = self.ops.get_vcp(&ident, VCP_BRIGHTNESS).await.ok();

        // Power: 0xD6 only exists on displays that advertised D6 support
        // during probe.  Reading on a non-D6 display returns an error; we
        // surface the `power` field as `None` in that case so the
        // brightness read still ships in the report.
        //
        // Map the VCP value to `PowerState`:
        // - 0x01 → On
        // - any other readable value → Standby (0x02 standby, 0x03 suspend,
        //   0x04 off-soft, 0x05 off-hard — all "panel not in use" family)
        // - read error / unsupported → None
        let power = match self.ops.get_vcp(&ident, VCP_POWER).await {
            Ok(D6_ON) => Some(PowerState::On),
            Ok(_) => Some(PowerState::Standby),
            Err(_) => None,
        };

        // Return Some only when at least one read succeeded; an empty
        // PanelState would defeat the exercise's "did anything change?"
        // comparison.
        if brightness.is_none() && power.is_none() {
            None
        } else {
            Some(PanelState { power, brightness })
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use crate::vcp_ops::FakeVcp;
    use crate::vcp_ops::VcpDisplayInfo;

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
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
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
    async fn probe_matcher_substring() {
        let fake = Arc::new(two_display_vcp());
        let mut ctrl = DdcciController::with_ops(
            Some("DELL".into()),
            80,
            BlankMode::PowerOff,
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
            BlankMode::PowerOff,
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
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
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
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
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
        let mut ctrl2 = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake2) as Arc<dyn VcpOps>,
        );
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
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        // Now blank with BrightnessZero — needs get(0x10) then set(0x10, 0)
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(75));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));

        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();

        let state = ctrl.state.lock().unwrap();
        assert_eq!(state.saved_brightness, Some(75));
        // last_blank_mode must be recorded on success so `wake()` can pick
        // the brightness-restore path.
        assert_eq!(state.last_blank_mode, Some(BlankMode::BrightnessZero));

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

    /// Must 1 regression: a second blank while already blanked must NOT
    /// overwrite `saved_brightness` with the current (zero) value.
    #[tokio::test]
    async fn blank_twice_does_not_overwrite_saved_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        // First blank at 75 → saves 75, sets to 0.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(75));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(
            ctrl.state.lock().unwrap().saved_brightness,
            Some(75),
            "first blank should save 75"
        );

        // Second blank reads current=0, sets to 0 again — must NOT clobber.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(0));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(
            ctrl.state.lock().unwrap().saved_brightness,
            Some(75),
            "second blank must NOT overwrite saved_brightness with 0"
        );

        // Wake restores 75, not 0.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 75, Ok(()));
        ctrl.wake().await.unwrap();
    }

    #[tokio::test]
    async fn wake_restores_saved_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
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

    /// Must 2: wake clears `saved_brightness` so the next blank re-saves fresh.
    #[tokio::test]
    async fn wake_clears_saved_for_next_cycle() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        // First blank at 75.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(75));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(ctrl.state.lock().unwrap().saved_brightness, Some(75));

        // Wake clears saved.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 75, Ok(()));
        ctrl.wake().await.unwrap();
        assert!(
            ctrl.state.lock().unwrap().saved_brightness.is_none(),
            "wake should clear saved_brightness"
        );

        // Second blank at 90 → re-saves 90 (not stale 75).
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(90));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(
            ctrl.state.lock().unwrap().saved_brightness,
            Some(90),
            "second blank should re-save fresh brightness 90"
        );
    }

    /// After a daemon restart the saved brightness is gone (in-memory
    /// state is lost). For a `BrightnessZero`-primary display, the wake
    /// path must still bring the panel to the operator-tuned
    /// `restore_brightness` config default (restart-safety-net).
    #[tokio::test]
    async fn wake_without_saved_uses_config_default() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            55,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        // Wake without any blank first — last_blank_mode is None, primary
        // is BrightnessZero, saved_brightness is None → fall through to
        // restore_brightness (55).
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
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(
            err.error.contains("not supported"),
            "error should mention D6 not supported: {err}"
        );
    }

    #[tokio::test]
    async fn wake_after_power_off_sends_d6_on_only() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        // Blank with PowerOff
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_OFF, Ok(()));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();

        // Wake — D6_ON only. A PowerOff blank never touched brightness, so
        // wake must not write VCP 0x10 (that write clobbers whatever
        // brightness the operator set between blank and wake — the
        // live-caught bug this fix addresses).
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_ON, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0xD6") && l.contains('1')),
            "wake should call D6_ON: {log:?}"
        );
        assert!(
            !log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10")),
            "wake after a PowerOff blank must NOT write brightness (0x10): {log:?}"
        );
    }

    /// RED-first regression for the live-caught bug: `wake()` used to
    /// unconditionally restore brightness even when the preceding blank
    /// was `PowerOff` (which never touched brightness), silently
    /// clobbering the operator's brightness (e.g. 100 → 80 config
    /// default) on every presence-driven wake of a power-off display.
    #[tokio::test]
    async fn power_off_wake_does_not_write_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_OFF, Ok(()));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();

        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_ON, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            !log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10")),
            "power-off wake must not write brightness VCP 0x10: {log:?}"
        );
    }

    /// Restart-fallback: a fresh controller (no `last_blank_mode` yet)
    /// whose configured primary mode is `PowerOff` must wake via D6-on
    /// only — the same as an in-session `PowerOff` blank/wake cycle.
    #[tokio::test]
    async fn restart_fallback_power_off_primary_wake_does_not_write_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        // No blank() call — simulates a daemon restart mid-blank.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_ON, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            !log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10")),
            "restart fallback to PowerOff primary must not write brightness: {log:?}"
        );
    }

    /// Restart-fallback: a fresh controller (no `last_blank_mode` yet)
    /// whose configured primary mode is `BrightnessZero` must still wake
    /// via the config-default brightness restore.
    #[tokio::test]
    async fn restart_fallback_brightness_zero_primary_wake_restores_config_default() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            62,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
        );
        ctrl.probe().await.unwrap();

        // No blank() call — simulates a daemon restart mid-blank.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 62, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains("62")),
            "restart fallback to BrightnessZero primary should restore config default 62: {log:?}"
        );
    }
}
