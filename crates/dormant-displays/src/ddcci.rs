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

use crate::ddc_lock::{PanelLock, PanelLocks};
#[cfg(target_os = "linux")]
use crate::vcp_ops::RealVcp;
use crate::vcp_ops::{VcpOps, VcpPriority};

// ── VCP code constants ─────────────────────────────────────────────────────────

/// VCP code for brightness (continuous, 0–100).
const VCP_BRIGHTNESS: u8 = 0x10;

/// VCP code for power control (D6 — optional feature).
const VCP_POWER: u8 = 0xD6;

/// VCP code for cumulative display usage time (MCCS "Display Usage Time").
/// The value can exceed 16 bits, so it is read via
/// [`crate::vcp_ops::VcpOps::get_vcp_raw`] and decoded per
/// [`decode_usage_hours`] rather than the plain `get_vcp` u16 path.
const VCP_USAGE_HOURS: u8 = 0xC0;

/// D6 value: display on.
const D6_ON: u16 = 0x01;

/// D6 value: display off.
const D6_OFF: u16 = 0x05;

// ── DdcciController ────────────────────────────────────────────────────────────

/// Internal mutable state discovered during [`probe`].
#[derive(Debug, Default)]
struct DdcState {
    /// The ident string of the matched display (set during probe).
    ///
    /// Doubles as this controller's **canonical panel-lock key** (spec
    /// §4.3): ddc-hi's `Display::info` identifier string
    /// (`backend:id manufacturer model_name`) is already stable and unique
    /// per physical panel within a process, which is exactly the property
    /// [`crate::ddc_lock::PanelLocks::get`] needs — a real EDID-derived
    /// identity type does not exist elsewhere in this codebase, so re-using
    /// the existing matcher identity avoids inventing a second, redundant
    /// one. Two `DdcciController`s that probe to the same `ident_string`
    /// (e.g. across a config reload) resolve to the same `Arc<PanelLock>`
    /// by construction, via the shared `PanelLocks` registry.
    matched_ident: Option<String>,
    /// This panel's serialization lock, resolved from `matched_ident` via
    /// the shared [`PanelLocks`] registry during [`DdcciController::probe`].
    /// `None` until probed — mirrors `matched_ident`'s lifecycle exactly,
    /// so every VCP-touching method that checks one also has the other.
    panel_lock: Option<Arc<PanelLock>>,
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
    /// The process- (or test-)wide panel-lock registry. Stored here, not
    /// resolved to a single `Arc<PanelLock>` at construction, because the
    /// canonical key isn't known until [`probe`](DisplayController::probe)
    /// derives it from the matched display.
    locks: Arc<PanelLocks>,
    state: StdMutex<DdcState>,
}

impl DdcciController {
    /// Build a new `DdcciController` with real DDC/CI hardware access.
    ///
    /// `locks` is the daemon's single process-wide [`PanelLocks`] registry
    /// (spec §4.3) — shared across every controller and every config-reload
    /// generation so the same physical panel always resolves to the same
    /// `Arc<PanelLock>`.
    ///
    /// Only available on Linux — DDC/CI requires platform I²C support.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn new(
        matcher: Option<String>,
        restore_brightness: u8,
        configured_primary_mode: BlankMode,
        locks: &Arc<PanelLocks>,
    ) -> Self {
        Self {
            matcher,
            restore_brightness,
            configured_primary_mode,
            ops: Arc::new(RealVcp),
            locks: Arc::clone(locks),
            state: StdMutex::new(DdcState::default()),
        }
    }

    /// Build a `DdcciController` with a custom `VcpOps` implementation
    /// (used by tests to inject a fake). See [`Self::new`] for the `locks`
    /// contract.
    #[must_use]
    pub fn with_ops(
        matcher: Option<String>,
        restore_brightness: u8,
        configured_primary_mode: BlankMode,
        ops: Arc<dyn VcpOps>,
        locks: &Arc<PanelLocks>,
    ) -> Self {
        Self {
            matcher,
            restore_brightness,
            configured_primary_mode,
            ops,
            locks: Arc::clone(locks),
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

    /// Test-only accessor: the panel lock this controller resolved during
    /// probe (`None` if not yet probed). Used by the pin (e) test proving
    /// two controller instances built against the SAME [`PanelLocks`]
    /// registry, for the same panel identity, resolve to ONE
    /// `Arc<PanelLock>`.
    #[cfg(test)]
    pub(crate) fn panel_lock_for_test(&self) -> Option<Arc<PanelLock>> {
        self.state.lock().unwrap().panel_lock.clone()
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
        // Order matters (spec §4.3): enumerate → match → derive the
        // canonical key → resolve THIS panel's lock → only then touch the
        // bus. Deriving the key before any transaction means every VCP
        // call this controller ever makes — including the very first one,
        // right below — is already serialized against every other
        // controller instance for the same physical panel.
        let displays = self.ops.list_displays().await;
        let matched = Self::find_match(self.matcher.as_ref(), &displays)?;
        let panel_lock = self.locks.get(&matched);

        // Test D6 power control support — the first physical VCP
        // transaction, always at command priority (probe is a one-time
        // startup call, never periodic sampling).
        let d6_ok = self
            .ops
            .get_vcp(&matched, VCP_POWER, &panel_lock, VcpPriority::Command)
            .await
            .is_ok();

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
        state.panel_lock = Some(panel_lock);
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
        let (ident, lock, d6_supported) = {
            let state = self.state.lock().unwrap();
            let (ident, lock) = match (&state.matched_ident, &state.panel_lock) {
                (Some(id), Some(lock)) => (id.clone(), Arc::clone(lock)),
                _ => {
                    return Err(CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: controller not probed"),
                    });
                }
            };
            (ident, lock, state.d6_supported)
        };

        let result = match mode {
            BlankMode::BrightnessZero => {
                // Save current brightness, then set to 0. Both physical
                // transactions run at command priority — a blank is
                // always command-path work.
                let current = self
                    .ops
                    .get_vcp(&ident, VCP_BRIGHTNESS, &lock, VcpPriority::Command)
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to read brightness: {e}"),
                    })?;

                self.ops
                    .set_vcp(&ident, VCP_BRIGHTNESS, 0, &lock, VcpPriority::Command)
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to set brightness to 0: {e}"),
                    })?;

                // Only save on the FIRST blank — a second blank while already
                // blanked reads current=0 and would clobber the real saved
                // value, causing wake to restore 0 (stuck dark). A zero read
                // on the very first blank is also refused: a genuine zero
                // operator preference cannot be distinguished from blank
                // residue, and failing-toward-visible means the fallback
                // (config `restore_brightness`, validated ≥ 1) is always safe.
                let mut state = self.state.lock().unwrap();
                if state.saved_brightness.is_none() {
                    if current > 0 {
                        state.saved_brightness = Some(current);
                    } else {
                        tracing::debug!(
                            event = "brightness_zero_not_saved",
                            display = %ident,
                            "pre-blank brightness is 0 — not saving as operator level (fail-toward-visible)",
                        );
                    }
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
                    .set_vcp(&ident, VCP_POWER, D6_OFF, &lock, VcpPriority::Command)
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
            // A successful non-brightness blank (#33: mode switch) leaves
            // any previously saved brightness stale — it describes a panel
            // state from a DIFFERENT blank cycle. Clear it BEFORE recording
            // `last_blank_mode` so the next `BrightnessZero` blank re-saves
            // fresh instead of skipping the save (its `is_none()` guard)
            // and later restoring the stale value on wake. A FAILED write
            // must NOT reach this arm (guarded by `result.is_ok()` above):
            // the prior successful brightness-zero state may still
            // describe the physical panel and remains the safest wake
            // fallback.
            if mode != BlankMode::BrightnessZero {
                state.saved_brightness = None;
            }
            state.last_blank_mode = Some(mode);
        }
        result
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let (ident, lock, d6_supported, effective_mode) = {
            let state = self.state.lock().unwrap();
            let (ident, lock) = match (&state.matched_ident, &state.panel_lock) {
                (Some(id), Some(lock)) => (id.clone(), Arc::clone(lock)),
                _ => {
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
            (ident, lock, state.d6_supported, mode)
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
                // wake.) Wake is command-path work — the wake-latency
                // invariant (spec §9#1) is exactly this ONE transaction.
                if d6_supported {
                    self.ops
                        .set_vcp(&ident, VCP_POWER, D6_ON, &lock, VcpPriority::Command)
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
                    let _ = self
                        .ops
                        .set_vcp(&ident, VCP_POWER, D6_ON, &lock, VcpPriority::Command)
                        .await;
                }

                // Restore brightness.
                self.ops
                    .set_vcp(&ident, VCP_BRIGHTNESS, restore, &lock, VcpPriority::Command)
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
    /// Command priority: this is the trait's original readback, used by
    /// `dormantctl doctor --exercise`, which must not be starved by a
    /// concurrent sampler tick.  See [`Self::read_state_sampled`] for the
    /// sampler-priority variant.
    async fn read_state(&self) -> Option<dormant_core::traits::PanelState> {
        self.read_state_at(VcpPriority::Command).await
    }

    /// Read the panel state at **sampler priority** (spec §4.3) — used by
    /// the periodic wear-tracking poll, which must yield instantly to any
    /// command-path caller rather than make one wait behind it.  Same
    /// decode logic as [`Self::read_state`]; only the panel-lock
    /// acquisition strategy differs (see [`crate::vcp_ops::VcpPriority`]).
    /// A skipped or failed read collapses to `None` — indistinguishable
    /// from a real readback failure, and never logged as a display
    /// failure, matching [`Self::read_state`]'s existing error handling.
    async fn read_state_sampled(&self) -> Option<dormant_core::traits::PanelState> {
        self.read_state_at(VcpPriority::Sampler).await
    }

    /// Read the panel's cumulative usage-hours counter (VCP `0xC0`,
    /// MCCS "Display Usage Time").
    ///
    /// Command priority: usage-hours is read once, at ledger-creation
    /// (seeding) time — an operator- or daemon-startup-triggered
    /// command-path event, per the `ddc_lock` module's command/sampler
    /// classification, never a periodic sample.
    ///
    /// Returns `None` when the controller has not been probed, when the
    /// display doesn't support VCP `0xC0`, or on any read failure — the
    /// honest "couldn't seed" answer; the caller (wear-ledger seeding,
    /// task T7) falls back to an unseeded ledger rather than fabricating a
    /// value.
    async fn read_usage_hours(&self) -> Option<u32> {
        let (ident, lock) = {
            let state = self.state.lock().unwrap();
            match (&state.matched_ident, &state.panel_lock) {
                (Some(id), Some(lock)) => (id.clone(), Arc::clone(lock)),
                _ => return None,
            }
        };
        let raw = self
            .ops
            .get_vcp_raw(&ident, VCP_USAGE_HOURS, &lock, VcpPriority::Command)
            .await
            .ok()?;
        Some(decode_usage_hours(raw))
    }

    /// Stable panel identity (spec §3 / T7 review M1): the canonical
    /// panel-lock key resolved during `probe` — ddc-hi's `Display::info`
    /// ident string (`backend:id manufacturer model_name`), the same value
    /// `matched_ident` holds. This is bus-path-derived, NOT the EDID
    /// mfg/model/serial format the spec's `WearIdentity` doc names as the
    /// ideal (that would require deeper `ddc-hi` field access this
    /// controller does not yet parse — out of scope for this fix per the
    /// T7 review's priority adjudication A); it is nonetheless
    /// panel-derived rather than config-derived, which is the property
    /// that matters: a `[displays.*]` rename does not orphan the ledger.
    /// `None` before `probe()` has run.
    fn panel_identity(&self) -> Option<String> {
        self.state.lock().unwrap().matched_ident.clone()
    }
}

impl DdcciController {
    /// Shared implementation of [`DisplayController::read_state`] and
    /// [`DisplayController::read_state_sampled`] — identical decode logic,
    /// differing only in the panel-lock acquisition strategy `prio`
    /// selects. Keeping one implementation means the two trait methods can
    /// never drift apart on what "brightness" or "power" means.
    async fn read_state_at(&self, prio: VcpPriority) -> Option<dormant_core::traits::PanelState> {
        use dormant_core::traits::{PanelState, PowerState};

        let (ident, lock) = {
            let state = self.state.lock().unwrap();
            match (&state.matched_ident, &state.panel_lock) {
                (Some(id), Some(lock)) => (id.clone(), Arc::clone(lock)),
                _ => return None,
            }
        };

        // Brightness: 0x10 is continuous 0–100 on every DDC/CI monitor.
        let brightness = self
            .ops
            .get_vcp(&ident, VCP_BRIGHTNESS, &lock, prio)
            .await
            .ok();

        // Power: 0xD6 only exists on displays that advertised D6 support
        // during probe.  Reading on a non-D6 display returns an error; we
        // surface the `power` field as `None` in that case so the
        // brightness read still ships in the report.
        //
        // Map the VCP value to `PowerState`:
        // - 0x01 → On
        // - any other readable value → Standby (0x02 standby, 0x03 suspend,
        //   0x04 off-soft, 0x05 off-hard — all "panel not in use" family)
        // - read error / unsupported (including a sampler-priority skip) →
        //   None
        let power = match self.ops.get_vcp(&ident, VCP_POWER, &lock, prio).await {
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

/// Decode a VCP `0xC0` raw reply `[mh, ml, sh, sl]` into a usage-hours
/// count.
///
/// `mh` (max-high) is unused by this feature's MCCS encoding. `ml`
/// (max-low) carries the high-order byte of a value that can exceed 16
/// bits — this is why usage-hours is read via
/// [`crate::vcp_ops::VcpOps::get_vcp_raw`] rather than the plain `get_vcp`
/// `u16` path, which cannot represent more than 65535.
fn decode_usage_hours(raw: [u8; 4]) -> u32 {
    let [_mh, ml, sh, sl] = raw;
    (u32::from(ml) << 16) | (u32::from(sh) << 8) | u32::from(sl)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use std::time::{Duration, Instant};

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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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

    /// RED regression for #33: a mode switch between two `BrightnessZero`
    /// blanks must not leak stale `saved_brightness` across the
    /// intervening successful non-brightness blank. Sequence:
    /// `BrightnessZero` at 75 -> successful `PowerOff` -> `BrightnessZero`
    /// at 60 -> wake. Wake must restore the FRESH 60 (saved by the second
    /// brightness-zero blank), not the stale 75 saved before the mode
    /// switch.
    #[tokio::test]
    async fn mode_switch_clears_stale_saved_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &PanelLocks::new(),
        );
        ctrl.probe().await.unwrap();

        // First BrightnessZero blank at 75 -> saves 75.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(75));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(ctrl.state.lock().unwrap().saved_brightness, Some(75));

        // Mode switch: a successful PowerOff blank must clear the stale
        // saved_brightness.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_OFF, Ok(()));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            ctrl.state.lock().unwrap().saved_brightness,
            None,
            "a successful non-brightness blank must clear stale saved_brightness"
        );

        // Second BrightnessZero blank at 60 -- must re-save fresh (60),
        // not skip saving because a stale saved_brightness was still Some.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(60));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(ctrl.state.lock().unwrap().saved_brightness, Some(60));

        // Wake -- must restore 60, not stale 75. Only 60 is scripted: if
        // the fake observes a set_vcp(0x10, 75) call instead, wake()
        // surfaces that as a CmdFailure and this unwrap panics.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_ON, Ok(()));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 60, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains("60")),
            "wake should restore fresh 60, not stale 75: {log:?}"
        );
    }

    /// Failure sibling of `mode_switch_clears_stale_saved_brightness`: if
    /// the intervening non-brightness blank FAILS (D6 write error), the
    /// prior successful brightness-zero save must survive -- it may still
    /// describe the physical panel and remains the safest wake fallback.
    #[tokio::test]
    async fn failed_mode_switch_preserves_saved_brightness() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &PanelLocks::new(),
        );
        ctrl.probe().await.unwrap();

        // BrightnessZero blank at 75 -> saves 75.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(75));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(ctrl.state.lock().unwrap().saved_brightness, Some(75));

        // Failed PowerOff blank -- the D6-off write itself errors.
        fake.expect_set(
            "i2c-dev:56 DEL DELL U2723QE",
            VCP_POWER,
            D6_OFF,
            Err("write failed".into()),
        );
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(
            err.error.contains("failed to set power off"),
            "blank failure should mention power-off write: {err}"
        );

        // saved_brightness (and last_blank_mode) must survive the failed
        // blank -- a failed D6 write must NOT clear the safest fallback.
        assert_eq!(
            ctrl.state.lock().unwrap().saved_brightness,
            Some(75),
            "a failed non-brightness blank must NOT clear saved_brightness"
        );
        assert_eq!(
            ctrl.state.lock().unwrap().last_blank_mode,
            Some(BlankMode::BrightnessZero),
            "a failed blank must not overwrite last_blank_mode"
        );

        // Wake still restores 75.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_ON, Ok(()));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 75, Ok(()));
        ctrl.wake().await.unwrap();
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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

    /// Pin (#32): if the D6-on write itself fails on wake, that failure
    /// must propagate as a `CmdFailure` carrying `E_DISPLAY_IO` and the
    /// underlying write error -- not be swallowed. Only one D6-on write
    /// should be attempted, and (since the preceding blank was `PowerOff`,
    /// which never touches brightness) no brightness write should happen
    /// either.
    #[tokio::test]
    async fn wake_after_power_off_propagates_d6_on_error() {
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
            &PanelLocks::new(),
        );
        ctrl.probe().await.unwrap();

        // Blank with PowerOff -- succeeds.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, D6_OFF, Ok(()));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();

        // Wake's D6-on write fails.
        fake.expect_set(
            "i2c-dev:56 DEL DELL U2723QE",
            VCP_POWER,
            D6_ON,
            Err("write failed".into()),
        );
        let err = ctrl.wake().await.unwrap_err();
        assert!(
            err.error.contains("E_DISPLAY_IO"),
            "wake failure must carry E_DISPLAY_IO: {err}"
        );
        assert!(
            err.error.contains("failed to set power on: write failed"),
            "wake failure must surface the underlying write error: {err}"
        );

        let log = fake.take_call_log();
        // The blank() call above also logs a `0xD6` write (D6_OFF, value
        // 5); filter on the D6_ON value (1) specifically, same idiom as
        // `wake_after_power_off_sends_d6_on_only`, so this only counts
        // wake's D6-on attempt.
        assert_eq!(
            log.iter()
                .filter(|l| l.contains("set_vcp") && l.contains("0xD6") && l.contains('1'))
                .count(),
            1,
            "wake should attempt the D6-on write exactly once: {log:?}"
        );
        assert!(
            !log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10")),
            "a failed D6-on write must not fall through to a brightness write: {log:?}"
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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
            &PanelLocks::new(),
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

    // ── T5: usage-hours decode (pin test 3) ──────────────────────────────────

    #[test]
    fn decode_usage_hours_probe_ground_truth() {
        // spec §4.3 pin: the exact raw reply captured against real hardware
        // during the 2026-07-09 probe.
        assert_eq!(decode_usage_hours([0x00, 0x00, 0x03, 0xC6]), 966);
    }

    #[test]
    fn decode_usage_hours_wide_decode() {
        // A value that would overflow `u16` (hence `get_vcp_raw`, not
        // `get_vcp`), exercising the `ml` high-order byte.
        assert_eq!(decode_usage_hours([0x00, 0x01, 0x00, 0x00]), 65536);
    }

    #[tokio::test]
    async fn read_usage_hours_decodes_scripted_c0_via_controller() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let locks = PanelLocks::new();
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &locks,
        );
        ctrl.probe().await.unwrap();

        fake.expect_get_raw(
            "i2c-dev:56 DEL DELL U2723QE",
            VCP_USAGE_HOURS,
            Ok([0x00, 0x00, 0x03, 0xC6]),
        );
        assert_eq!(ctrl.read_usage_hours().await, Some(966));
    }

    #[tokio::test]
    async fn read_usage_hours_none_when_not_probed() {
        let fake = Arc::new(single_display_vcp());
        let ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &PanelLocks::new(),
        );
        assert_eq!(ctrl.read_usage_hours().await, None);
    }

    // ── T7 fix M1: panel_identity ────────────────────────────────────────────

    /// RED-first (T7 review M1): before probe, no canonical key has been
    /// resolved yet — `panel_identity()` must be the honest `None`, not a
    /// fabricated value.
    #[tokio::test]
    async fn panel_identity_none_before_probe() {
        let fake = Arc::new(single_display_vcp());
        let ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &PanelLocks::new(),
        );
        assert_eq!(ctrl.panel_identity(), None);
    }

    /// After probe, `panel_identity()` returns the same canonical
    /// bus-path-derived key as `matched_ident`/the panel lock — panel-
    /// derived, not config-derived, so a wear ledger keyed on this survives
    /// a `[displays.*]` config rename.
    #[tokio::test]
    async fn panel_identity_returns_canonical_key_after_probe() {
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
            &PanelLocks::new(),
        );
        ctrl.probe().await.unwrap();
        assert_eq!(
            ctrl.panel_identity().as_deref(),
            Some("i2c-dev:56 DEL DELL U2723QE"),
            "panel_identity must match the canonical key derived during probe"
        );
    }

    // ── T5: canonical key + lock ordering (pin test 4, pin (e)) ─────────────

    /// Pin test 4: probe derives the canonical key and resolves this
    /// panel's lock BEFORE its first physical VCP transaction. Proven by
    /// pre-holding the SAME key's lock (via the same `PanelLocks` registry,
    /// resolved independently of the controller) and observing that
    /// `probe()`'s first transaction genuinely blocks on it — if probe
    /// touched the bus before resolving the canonical key (or resolved a
    /// *different* lock), this transaction would run immediately and the
    /// elapsed-time assertion below would fail.
    #[tokio::test]
    async fn probe_derives_canonical_key_and_serializes_first_transaction_on_it() {
        let ident = "i2c-dev:56 DEL DELL U2723QE";
        let locks = PanelLocks::new();
        let pre_held = locks.get(ident);
        let guard = pre_held.command_guard();

        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get(ident, VCP_POWER, Ok(D6_ON));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::PowerOff,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &locks,
        );

        let probe_task = tokio::spawn(async move {
            ctrl.probe().await.unwrap();
            ctrl
        });
        // Let the spawned probe reach (and block on) the pre-held lock.
        tokio::time::sleep(Duration::from_millis(40)).await;
        let start = Instant::now();
        drop(guard);
        let ctrl = probe_task.await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "probe should complete promptly once the pre-held lock is released: {elapsed:?}"
        );
        assert!(
            ctrl.panel_lock_for_test().is_some(),
            "probe must have resolved and stored a panel lock"
        );
        assert!(
            Arc::ptr_eq(&pre_held, &ctrl.panel_lock_for_test().unwrap()),
            "probe's resolved lock must be the SAME Arc<PanelLock> instance \
             pre-held under the identical canonical key — proves the key \
             (ident string) is derived and looked up via the shared \
             PanelLocks registry BEFORE any transaction runs"
        );
    }

    /// Pin (e): two controller instances built from the SAME `PanelLocks`
    /// registry for the same panel identity resolve to ONE `Arc<PanelLock>`
    /// — the property a config reload depends on (spec §4.3: an
    /// old-generation controller and a new-generation controller for the
    /// same physical panel must serialize against each other).
    #[tokio::test]
    async fn two_controllers_same_registry_and_identity_share_one_lock() {
        let locks = PanelLocks::new();
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl1 = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &locks,
        );
        let mut ctrl2 = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &locks,
        );
        ctrl1.probe().await.unwrap();
        ctrl2.probe().await.unwrap();

        assert!(Arc::ptr_eq(
            &ctrl1.panel_lock_for_test().unwrap(),
            &ctrl2.panel_lock_for_test().unwrap(),
        ));
    }

    // ── T5: scripted-panic then successful wake (pin (d)) ────────────────────

    #[tokio::test]
    async fn scripted_panic_during_blank_then_successful_wake() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let locks = PanelLocks::new();
        let mut ctrl = DdcciController::with_ops(
            None,
            55,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &locks,
        );
        ctrl.probe().await.unwrap();

        // Blank's brightness-save read panics mid-transaction.
        fake.expect_get_panic("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS);
        let err = ctrl.blank(BlankMode::BrightnessZero).await.unwrap_err();
        assert!(
            err.error.contains("E_DISPLAY_IO"),
            "panicked transaction must surface as a normal CmdFailure, not a \
             process panic: {err}"
        );
        // Pin catch_unwind specifically (not just tokio's own per-task
        // panic isolation, which would ALSO turn the panic into an Err via
        // a raw "spawn_blocking join error: ..." join error and could mask
        // catch_unwind's absence): the error must carry the domain-specific
        // VCP_PANIC sentinel, not a raw join-error string.
        assert!(
            err.error.contains(crate::vcp_ops::VCP_PANIC),
            "panicked transaction must surface the VCP_PANIC sentinel \
             (proves catch_unwind, not just tokio's task-level panic \
             isolation, converted it): {err}"
        );

        // The panel lock must still be usable — a fully independent
        // subsequent wake succeeds normally (no blank was recorded, so
        // wake falls through to the configured primary / restore_brightness
        // default).
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 55, Ok(()));
        ctrl.wake().await.unwrap();
    }

    // ── T5: relational wake-latency bound (pin (c), P4) ───────────────────────

    /// Wake-path invariant (spec §9#1): a wake's panel-lock wait is bounded
    /// by AT MOST one in-flight physical VCP transaction it collides with —
    /// here, a slow sampler read. Relational (P4): compares wake's elapsed
    /// time against the sampler's OWN measured elapsed time (plus slack),
    /// never a bare wall-clock constant.
    #[tokio::test]
    async fn wake_latency_bounded_by_one_inflight_sampler_read() {
        let ident = "i2c-dev:56 DEL DELL U2723QE";
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get(ident, VCP_POWER, Err("no".into()));
            f
        });
        let locks = PanelLocks::new();
        let mut ctrl = DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &locks,
        );
        ctrl.probe().await.unwrap();
        let ctrl = Arc::new(ctrl);

        // Script the sampler's slow brightness read + its (unsupported)
        // power read.
        fake.expect_get_delay(ident, VCP_BRIGHTNESS, Duration::from_millis(80));
        fake.expect_get(ident, VCP_BRIGHTNESS, Ok(50));
        fake.expect_get(ident, VCP_POWER, Err("no".into()));

        let sample_ctrl = Arc::clone(&ctrl);
        let sample_task = tokio::spawn(async move { sample_ctrl.read_state_sampled().await });
        // Let the sampler grab the lock and enter its delayed read before
        // the wake arrives.
        tokio::time::sleep(Duration::from_millis(15)).await;

        fake.expect_set(ident, VCP_BRIGHTNESS, 80, Ok(()));
        let wake_start = Instant::now();
        ctrl.wake().await.unwrap();
        let wake_elapsed = wake_start.elapsed();

        sample_task.await.unwrap();
        let sampler_read_elapsed = fake
            .take_last_get_elapsed()
            .expect("sampler's delayed read must have measured its own elapsed time");

        let read_timeout = Duration::from_secs(5);
        assert!(
            wake_elapsed < sampler_read_elapsed + Duration::from_millis(200),
            "wake_elapsed {wake_elapsed:?} must be bounded by the single \
             in-flight sampler transaction {sampler_read_elapsed:?} + slack \
             — not by any additional unrelated wait"
        );
        assert!(
            wake_elapsed < read_timeout,
            "wake_elapsed {wake_elapsed:?} must stay well under the command timeout"
        );
    }

    /// Daemon restart while the panel is dimmed (brightness physically at 0)
    /// — the first blank reads current=0. A zero reading can be either
    /// blank residue or a genuine operator preference; to fail-toward-visible
    /// the controller refuses to save it, leaving `saved_brightness == None`
    /// so wake falls through to `restore_brightness` (77 here,
    /// operator-configured) — never 0. Config validation rejects a zero
    /// restore default so the fallback is always at least 1.
    #[tokio::test]
    async fn brightness_zero_not_saved_wake_restores_config_default() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            77, // operator-configured restore (non-default to prove config path)
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &PanelLocks::new(),
        );
        ctrl.probe().await.unwrap();

        // Blank reads 0 → saves nothing, sets to 0.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(0));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();

        // Wake — restores config default 77, not 0.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 77, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        // Decisive: wake wrote 77 (config default), not a poisoned restore of 0.
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains("77")),
            "wake must restore config default 77, not 0: {log:?}"
        );
        let wake_sets: Vec<&String> = log
            .iter()
            .filter(|l| l.contains("set_vcp") && l.contains("0x10"))
            .collect();
        assert_eq!(
            wake_sets.len(),
            2,
            "one blank set (→0) + one wake set (→77)"
        );
        assert!(wake_sets[1].contains("77"), "second VCP set must be 77");
        // Corroborating: the zero reading was not saved as an operator level.
        assert!(
            ctrl.state.lock().unwrap().saved_brightness.is_none(),
            "pre-blank brightness 0 must not be saved as operator level"
        );
    }

    /// Healthy path: nonzero pre-blank reading IS the operator-chosen level —
    /// saved on first blank and restored on wake.
    #[tokio::test]
    async fn nonzero_brightness_saved_and_restored_on_wake() {
        let fake = Arc::new({
            let f = single_display_vcp();
            f.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_POWER, Err("no".into()));
            f
        });
        let mut ctrl = DdcciController::with_ops(
            None,
            77, // config default — irrelevant; saved value wins
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn VcpOps>,
            &PanelLocks::new(),
        );
        ctrl.probe().await.unwrap();

        // Blank reads 55 → saves 55, sets to 0.
        fake.expect_get("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, Ok(55));
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 0, Ok(()));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(
            ctrl.state.lock().unwrap().saved_brightness,
            Some(55),
            "nonzero pre-blank brightness must be saved"
        );

        // Wake restores saved 55.
        fake.expect_set("i2c-dev:56 DEL DELL U2723QE", VCP_BRIGHTNESS, 55, Ok(()));
        ctrl.wake().await.unwrap();

        let log = fake.take_call_log();
        assert!(
            log.iter()
                .any(|l| l.contains("set_vcp") && l.contains("0x10") && l.contains("55")),
            "wake must restore saved brightness 55: {log:?}"
        );
    }
}
