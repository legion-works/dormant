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

use crate::vcp_ops::{RealVcp, VcpOps};

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
#[derive(Debug, Clone, Default)]
struct DdcState {
    /// The ident string of the matched display (set during probe).
    matched_ident: Option<String>,
    /// Whether VCP code 0xD6 (power) is supported.
    d6_supported: bool,
    /// Saved brightness value from the first `blank(BrightnessZero)` call.
    /// `None` means no blank has happened yet (wake uses config default).
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
/// `ident_string` contains `pattern` as a **case-sensitive substring** is
/// selected. Zero matches or multiple matches (without a matcher) produce a
/// probe error.
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

    /// Build a `DdcciController` with a custom `VcpOps` implementation
    /// (used by tests to inject a fake).
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

        match mode {
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
        }
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let (ident, d6_supported, restore) = {
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
            let saved = state.saved_brightness;
            (
                ident,
                state.d6_supported,
                saved.unwrap_or(u16::from(self.restore_brightness)),
            )
        };

        // If D6 is supported, try to power on first (ignore error — the
        // brightness restore is the primary wake mechanism).
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

        // Clear saved_brightness so the NEXT blank cycle re-saves fresh.
        // Without this, a user who manually raises brightness between cycles
        // gets a stale restore.
        let mut state = self.state.lock().unwrap();
        state.saved_brightness = None;

        Ok(())
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

    /// Must 1 regression: a second blank while already blanked must NOT
    /// overwrite `saved_brightness` with the current (zero) value.
    #[tokio::test]
    async fn blank_twice_does_not_overwrite_saved_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
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

    /// Must 2: wake clears `saved_brightness` so the next blank re-saves fresh.
    #[tokio::test]
    async fn wake_clears_saved_for_next_cycle() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(None, 80, Arc::clone(&fake) as Arc<dyn VcpOps>);
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
