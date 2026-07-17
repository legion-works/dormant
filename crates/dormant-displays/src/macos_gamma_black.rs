//! `macos-gamma-black` display controller — blanks a macOS display by
//! writing an all-zero Quartz gamma table (`CGSetDisplayTransferByTable`)
//! and wakes it by replaying the exact table captured just before the
//! blank.
//!
//! ## Why gamma, not brightness or power?
//!
//! macOS has no public per-display "set brightness to 0" or "power off"
//! API reachable without a kernel extension or a private framework beyond
//! what this milestone vendors (see [`crate::ddcci`] for the DDC/CI path,
//! which some — but not all — external displays support). The Quartz
//! gamma-table API (`CoreGraphics`) is public, per-display, and universally
//! available: forcing every channel to `0.0` makes the panel emit black
//! without touching backlight or power state. This is the audio- and
//! source-preserving fallback for a macOS host with a display that either
//! has no DDC/CI support or where `ddcci` degrades unavailable (see the
//! chain-degradation test in this module's test suite).
//!
//! ## Selector contract (Task 4, ratified)
//!
//! This controller is addressed by a **stable, panel-derived selector**
//! string of the shape `cg:<lowercase-cfuuid>`, sourced from the display
//! config's `output` field and never the numeric `CGDirectDisplayID` (which
//! is not guaranteed stable across reconnects/reboots). Resolution from
//! selector to the transient numeric ID happens on every call via
//! [`GammaApi::resolve`] — see `crate::macos_display_catalog` for the real
//! (macOS-only) FFI implementation. There is deliberately **no main-display
//! fallback**: config validation (`dormant_core::config::validate`) hard-
//! rejects a `macos-gamma-black` display with no `output` field.
//!
//! ## First-blank-wins saved state
//!
//! [`GammaHoldRegistry`] is the daemon-lifetime store of "what table should
//! wake restore" per selector. The FIRST successful blank for a selector
//! captures the pre-blank table; every subsequent blank while that hold is
//! still occupied is a cheap idempotent no-op (it does not touch the API at
//! all — see `first_blank_wins_and_repeated_blank_never_saves_black`).
//! `wake()` clears the hold on a confirmed, successful replay so the next
//! blank re-captures fresh (mirrors `DdcciController`'s
//! `saved_brightness` lifecycle).
//!
//! ## Fail-toward-visible
//!
//! If the table read just before a blank is *already* black and this
//! controller holds no saved table for the selector, `blank()` refuses to
//! proceed (`E_DISPLAY_IO:` — see the `unowned_already_black_table_is_never_saved` test):
//! adopting "black" as the wake target would strand the panel dark forever
//! on the next wake. This mirrors `DdcciController::blank`'s refusal to
//! save a pre-blank brightness of `0`.
//!
//! ## No periodic reassertion, no `Drop` restore
//!
//! This controller writes the gamma table exactly on `blank()`/`wake()`
//! calls — nothing polls or reasserts it, and there is deliberately no
//! `Drop` implementation that would restore gamma when a controller
//! instance is dropped. A config reload drops the old generation's
//! controllers while the display should very much stay dark; a `Drop`-
//! based restore would flash the panel bright on every reload. (Task 8
//! adds the daemon-lifetime breadcrumb + explicit shutdown/emergency
//! restore paths — this module only owns the per-call gamma read/write
//! contract.)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use dormant_core::error::{DormantError, E_DISPLAY_IO};
use dormant_core::traits::{DisplayController, PanelState};
use dormant_core::types::{BlankMode, CmdFailure};

use crate::gamma_breadcrumb::GammaBreadcrumb;

/// Numeric Core Graphics display identifier (`CGDirectDisplayID` in Apple's
/// headers). Defined platform-neutrally — not behind
/// `#[cfg(target_os = "macos")]` — so [`GammaApi`], [`GammaTable`], and
/// every test in this module compile and run on any host, including a
/// Linux sandbox that can never build the real FFI backend in
/// `crate::macos_display_catalog`.
#[allow(non_camel_case_types)]
pub type CGDirectDisplayID = u32;

/// Epsilon below which a gamma channel sample is treated as `0.0` for
/// [`GammaTable::is_black`] purposes. Real hardware readbacks of a
/// zero-write can carry tiny floating-point noise; `0.0` is otherwise wire-
/// exact for a table this controller wrote itself.
pub const GAMMA_EPSILON: f32 = 1e-4;

// ── GammaError ───────────────────────────────────────────────────────────

/// A typed failure from a [`GammaApi`] call — the resolve/read/write
/// surface's own error type, kept distinct from [`DormantError`] /
/// [`CmdFailure`] so [`GammaApi`] stays a narrow, dependency-free trait
/// that both the real macOS FFI backend and the test `FakeGammaApi` can
/// implement without reaching into `dormant-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GammaError(pub String);

impl std::fmt::Display for GammaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GammaError {}

impl From<&str> for GammaError {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for GammaError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ── GammaTable ───────────────────────────────────────────────────────────

/// A per-channel Quartz gamma table: one `f32` sample per index, per
/// red/green/blue channel.
///
/// `PartialEq` is a plain per-element comparison — a table this controller
/// reads back is expected to match, bit-for-bit, a table it (or the real
/// hardware) previously wrote; there is no tolerance-based `Eq`, only
/// [`Self::is_black`]'s epsilon check, which has a narrower, documented
/// purpose (detecting the panel is dark, not comparing two arbitrary
/// tables).
#[derive(Debug, Clone, PartialEq)]
pub struct GammaTable {
    /// Red channel samples.
    pub red: Vec<f32>,
    /// Green channel samples.
    pub green: Vec<f32>,
    /// Blue channel samples.
    pub blue: Vec<f32>,
}

impl GammaTable {
    /// Build an all-zero table of `len` samples per channel — the blank
    /// target this controller writes on every `blank(BrightnessZero)`.
    #[must_use]
    pub fn black(len: usize) -> Self {
        Self {
            red: vec![0.0; len],
            green: vec![0.0; len],
            blue: vec![0.0; len],
        }
    }

    /// Build a simple ascending-ramp table of `len` samples per channel
    /// (`0.0..=1.0`, identical across R/G/B) — a plausible non-black
    /// "identity-ish" table used by tests to stand in for whatever profile
    /// the real hardware was showing before a blank. `len <= 1` produces a
    /// single `1.0` sample (there is no meaningful ramp with fewer than two
    /// points, and `1.0` is unambiguously non-black).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "test/fixture table sizes are small (hundreds of samples); \
                  the usize->f32 ramp index conversion never approaches f32's \
                  24-bit exact-integer range in practice"
    )]
    pub fn linear(len: usize) -> Self {
        let ramp: Vec<f32> = if len <= 1 {
            vec![1.0; len]
        } else {
            let denom = (len - 1) as f32;
            (0..len).map(|i| i as f32 / denom).collect()
        };
        Self {
            red: ramp.clone(),
            green: ramp.clone(),
            blue: ramp,
        }
    }

    /// Number of samples per channel (channels are validated equal-length
    /// by [`Self::validate`]; this reads the red channel as the canonical
    /// length).
    #[must_use]
    pub fn len(&self) -> usize {
        self.red.len()
    }

    /// True when every channel has zero samples.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.red.is_empty()
    }

    /// True when every sample, on every channel, is within
    /// [`GAMMA_EPSILON`] of `0.0`.
    #[must_use]
    pub fn is_black(&self) -> bool {
        self.red
            .iter()
            .chain(self.green.iter())
            .chain(self.blue.iter())
            .all(|v| v.abs() <= GAMMA_EPSILON)
    }

    /// Validate structural invariants: channels are equal length, nonempty,
    /// and every sample is finite (no NaN/Inf). Called on every table this
    /// controller reads from [`GammaApi`] before holding or replaying it —
    /// a corrupt readback must never become a saved wake target.
    ///
    /// # Errors
    ///
    /// Returns a [`GammaError`] describing the first violated invariant.
    pub fn validate(&self) -> Result<(), GammaError> {
        if self.red.is_empty() || self.green.is_empty() || self.blue.is_empty() {
            return Err(GammaError::from("gamma table has an empty channel"));
        }
        if self.red.len() != self.green.len() || self.red.len() != self.blue.len() {
            return Err(GammaError::from(
                "gamma table channels have mismatched lengths",
            ));
        }
        if self
            .red
            .iter()
            .chain(self.green.iter())
            .chain(self.blue.iter())
            .any(|v| !v.is_finite())
        {
            return Err(GammaError::from("gamma table contains a non-finite sample"));
        }
        Ok(())
    }
}

// ── GammaApi ─────────────────────────────────────────────────────────────

/// Abstract Quartz gamma-table operations — real (macOS FFI) or fake.
///
/// Kept narrow and synchronous (unlike [`crate::vcp_ops::VcpOps`], these
/// Quartz calls are cheap, local, in-process reads/writes — no bus I/O, no
/// network — so there is no need for `spawn_blocking`/`async`).
pub trait GammaApi: Send + Sync {
    /// Resolve a stable `cg:<uuid>` selector to the transient numeric
    /// display ID currently backing it.
    ///
    /// # Errors
    ///
    /// Returns a [`GammaError`] when no online display currently matches
    /// the selector (disconnected, or the UUID is simply unknown).
    fn resolve(&self, selector: &str) -> Result<CGDirectDisplayID, GammaError>;

    /// Read the display's current gamma table.
    ///
    /// # Errors
    ///
    /// Returns a [`GammaError`] on any Quartz failure or an invalid/gone
    /// display ID.
    fn read_table(&self, display: CGDirectDisplayID) -> Result<GammaTable, GammaError>;

    /// Write `table` as the display's gamma table.
    ///
    /// # Errors
    ///
    /// Returns a [`GammaError`] on any Quartz failure or an invalid/gone
    /// display ID.
    fn write_table(&self, display: CGDirectDisplayID, table: &GammaTable)
    -> Result<(), GammaError>;
}

// ── GammaHoldRegistry ────────────────────────────────────────────────────

/// Daemon-lifetime store of "what table should wake restore", keyed by the
/// **stable selector string** (`cg:<uuid>`) — never the transient numeric
/// `CGDirectDisplayID`, which is not guaranteed to survive a reconnect.
///
/// Each selector's hold is an independently-locked slot
/// (`Arc<Mutex<Option<GammaTable>>>`) so that blanking/waking one display
/// never blocks a concurrent blank/wake on a different display sharing the
/// same registry — the same independence property
/// [`crate::ddc_lock::PanelLocks`] gives DDC/CI panels.
#[derive(Default)]
pub struct GammaHoldRegistry {
    holds: StdMutex<HashMap<String, Arc<StdMutex<Option<GammaTable>>>>>,
}

impl GammaHoldRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create this selector's independently-locked slot.
    fn slot(&self, selector: &str) -> Arc<StdMutex<Option<GammaTable>>> {
        let mut holds = self
            .holds
            .lock()
            .expect("GammaHoldRegistry holds lock poisoned");
        Arc::clone(
            holds
                .entry(selector.to_string())
                .or_insert_with(|| Arc::new(StdMutex::new(None))),
        )
    }

    /// The currently-held wake-target table for `selector`, if any.
    ///
    /// # Panics
    ///
    /// Panics if the internal slot lock is poisoned (a prior holder panicked
    /// while holding it) — mirrors every other `.expect(...)`-guarded lock
    /// in this crate (e.g. `DdcciController`'s `state` mutex); a poisoned
    /// hold registry indicates a bug elsewhere, not a condition this method
    /// can recover from.
    #[must_use]
    pub fn saved(&self, selector: &str) -> Option<GammaTable> {
        self.slot(selector)
            .lock()
            .expect("gamma hold slot lock poisoned")
            .clone()
    }

    /// Occupy `selector`'s hold with `table` — called once, by the FIRST
    /// successful blank for this selector (first-blank-wins; callers must
    /// check [`Self::saved`] first and skip calling this again while a hold
    /// is occupied).
    fn set(&self, selector: &str, table: GammaTable) {
        *self
            .slot(selector)
            .lock()
            .expect("gamma hold slot lock poisoned") = Some(table);
    }

    /// Vacate `selector`'s hold — called after a confirmed, successful
    /// `wake()` replay so the next `blank()` re-captures a fresh table
    /// rather than replaying a stale one.
    fn clear(&self, selector: &str) {
        *self
            .slot(selector)
            .lock()
            .expect("gamma hold slot lock poisoned") = None;
    }
}

// ── MacosGammaBlackController ───────────────────────────────────────────

/// Literal controller name — grep-stable, matches the `macos-gamma-black`
/// config `type`.
const NAME: &str = "macos-gamma-black";

/// Display controller that blanks a macOS display by zeroing its Quartz
/// gamma table and wakes it by replaying the table captured just before
/// the blank.
///
/// Capability: [`BlankMode::BrightnessZero`] only — see the module docs for
/// why gamma is the audio/source-preserving fallback this controller
/// targets.
pub struct MacosGammaBlackController {
    /// Stable `cg:<uuid>` selector (Task 4 contract) — also the
    /// [`GammaHoldRegistry`] key and this controller's
    /// [`DisplayController::panel_identity`].
    selector: String,
    api: Arc<dyn GammaApi>,
    holds: Arc<GammaHoldRegistry>,
    /// Task 8 crash-insurance breadcrumb (`crate::gamma_breadcrumb`).
    /// `None` in the plain [`Self::with_api`] constructor (most of this
    /// module's own tests don't exercise breadcrumb persistence and would
    /// otherwise need a throwaway temp dir each); ALWAYS `Some` in
    /// production (see [`Self::new`], which always threads one in from the
    /// daemon's [`crate::registry::ControllerBuildContext`]).
    breadcrumb: Option<Arc<GammaBreadcrumb>>,
}

impl MacosGammaBlackController {
    /// Build a `MacosGammaBlackController` with a real macOS Quartz
    /// backend (see [`crate::macos_display_catalog::RealGammaApi`]).
    ///
    /// `holds` is the daemon's single process-wide [`GammaHoldRegistry`] —
    /// constructed once and reused across every reload generation so a
    /// panel's saved wake table survives a config reload (mirrors
    /// [`crate::ddc_lock::PanelLocks`]'s contract for `DdcciController`).
    /// `breadcrumb` is the daemon's single process-wide
    /// [`GammaBreadcrumb`] — same one-instance-per-daemon-lifetime contract
    /// as `holds` (both come from the same
    /// [`crate::registry::ControllerBuildContext`]).
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn new(
        selector: String,
        holds: Arc<GammaHoldRegistry>,
        breadcrumb: Arc<GammaBreadcrumb>,
    ) -> Self {
        Self::with_api_and_breadcrumb(
            selector,
            Arc::new(crate::macos_display_catalog::RealGammaApi),
            holds,
            breadcrumb,
        )
    }

    /// Build a `MacosGammaBlackController` with a custom [`GammaApi`]
    /// implementation and NO breadcrumb persistence (used by this module's
    /// own tests that aren't exercising the breadcrumb contract — see
    /// [`Self::with_api_and_breadcrumb`] for tests that are).
    #[must_use]
    pub fn with_api(
        selector: String,
        api: Arc<dyn GammaApi>,
        holds: Arc<GammaHoldRegistry>,
    ) -> Self {
        Self {
            selector,
            api,
            holds,
            breadcrumb: None,
        }
    }

    /// Build a `MacosGammaBlackController` with a custom [`GammaApi`]
    /// implementation AND breadcrumb persistence wired in (used by
    /// production `Self::new` — macOS-only, so not linkable from a doc
    /// build on this target — and by breadcrumb-focused tests).
    #[must_use]
    pub fn with_api_and_breadcrumb(
        selector: String,
        api: Arc<dyn GammaApi>,
        holds: Arc<GammaHoldRegistry>,
        breadcrumb: Arc<GammaBreadcrumb>,
    ) -> Self {
        Self {
            selector,
            api,
            holds,
            breadcrumb: Some(breadcrumb),
        }
    }

    /// Resolve this controller's selector, mapping a [`GammaError`] to a
    /// [`CmdFailure`] with the `E_DISPLAY_IO:` prefix — the one place both
    /// `blank()` and `wake()` turn a resolve failure into the command-path
    /// error shape.
    fn resolve(&self) -> Result<CGDirectDisplayID, CmdFailure> {
        self.api.resolve(&self.selector).map_err(|e| CmdFailure {
            controller: NAME.to_string(),
            error: format!(
                "{E_DISPLAY_IO}: failed to resolve display '{}': {e}",
                self.selector
            ),
        })
    }

    /// Build a [`CmdFailure`] with the `E_DISPLAY_IO:` prefix and this
    /// controller's name — the single formatting call site for every
    /// gamma I/O failure below. An associated function (no `&self`): the
    /// controller name is the literal [`NAME`] constant, not per-instance
    /// state.
    fn io_err(detail: impl std::fmt::Display) -> CmdFailure {
        CmdFailure {
            controller: NAME.to_string(),
            error: format!("{E_DISPLAY_IO}: {detail}"),
        }
    }
}

#[async_trait]
impl DisplayController for MacosGammaBlackController {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        vec![BlankMode::BrightnessZero]
    }

    async fn probe(&mut self) -> Result<(), DormantError> {
        self.api
            .resolve(&self.selector)
            .map(|_| ())
            .map_err(|e| DormantError::DisplayIo {
                controller: NAME.to_string(),
                detail: format!("failed to resolve display '{}': {e}", self.selector),
            })
    }

    async fn is_available(&self) -> bool {
        self.api.resolve(&self.selector).is_ok()
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        if mode != BlankMode::BrightnessZero {
            return Err(Self::io_err(format!("unsupported blank mode {mode:?}")));
        }

        let id = self.resolve()?;

        // First-blank-wins idempotency: a hold already occupied for this
        // selector means an earlier blank (in this process, this
        // generation) already captured the wake target and drove the
        // panel black. A repeated blank call — config-reload replay, a
        // duplicate rules-engine dispatch, whatever the caller — is a
        // cheap no-op that never touches the API, so it can never
        // re-capture (and clobber) the saved table with the panel's own
        // current (black) reading.
        if self.holds.saved(&self.selector).is_some() {
            return Ok(());
        }

        let current = self
            .api
            .read_table(id)
            .map_err(|e| Self::io_err(format!("failed to read gamma table: {e}")))?;
        current
            .validate()
            .map_err(|e| Self::io_err(format!("pre-blank gamma table readback is invalid: {e}")))?;

        // Fail-toward-visible: a table that is already black with no held
        // wake target means either an external actor already blanked the
        // panel, or this is stale residue from an unclean prior process —
        // either way, adopting "black" as what wake should restore would
        // strand the panel dark forever. Refuse ownership instead.
        if current.is_black() {
            return Err(Self::io_err(format!(
                "current gamma table for '{}' is already black and no saved table exists — \
                 refusing to adopt black as the wake target",
                self.selector
            )));
        }

        // Task 8 crash insurance: persist the breadcrumb BEFORE the first
        // LUT write below — a crash between this call and the write still
        // leaves a breadcrumb naming the selector about to go dark, never a
        // write with no breadcrumb behind it. A breadcrumb write failure
        // aborts the blank entirely (fail-safe: better to refuse a blank
        // than to gamma-black a panel with no crash-recovery marker).
        if let Some(bc) = &self.breadcrumb {
            bc.add_selector(&self.selector).map_err(|e| {
                Self::io_err(format!(
                    "failed to persist gamma-blank breadcrumb before write: {e}"
                ))
            })?;
        }

        let black = GammaTable::black(current.len());
        self.api
            .write_table(id, &black)
            .map_err(|e| Self::io_err(format!("failed to write black gamma table: {e}")))?;

        // Post-write confirmation (Task 3 partial-write contract, mirrors
        // `DdcciController::verify_brightness_zero_write`): a Quartz write
        // can silently fail to land. On confirmation failure, immediately
        // attempt to replay the pre-write table (the value that would have
        // become the saved hold had confirmation succeeded — or, if an
        // earlier blank in this session already occupies the hold, THAT
        // value instead) before returning the ORIGINAL confirmation error.
        match self.api.read_table(id) {
            Ok(confirm) if confirm.is_black() => {
                self.holds.set(&self.selector, current);
                Ok(())
            }
            confirm_result => {
                let mismatch_detail = match confirm_result {
                    Ok(t) => {
                        format!("post-write verification mismatch: table is not black ({t:?})")
                    }
                    Err(e) => format!("post-write verification read failed: {e}"),
                };
                let rollback_target = self.holds.saved(&self.selector).unwrap_or(current);
                let mut detail = format!(
                    "{E_DISPLAY_IO}: failed to verify gamma-black write on '{}': {mismatch_detail}",
                    self.selector
                );
                if let Err(rollback_e) = self.api.write_table(id, &rollback_target) {
                    use std::fmt::Write;
                    let _ = write!(detail, " (rollback also failed: {rollback_e})");
                }
                Err(CmdFailure {
                    controller: NAME.to_string(),
                    error: detail,
                })
            }
        }
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let id = self.resolve()?;

        let Some(saved) = self.holds.saved(&self.selector) else {
            // No hold occupied — either never blanked by this controller,
            // or already woken. Idempotent no-op: never call any system-
            // wide restore, only ever replay a per-display saved table
            // this controller itself owns.
            return Ok(());
        };

        self.api
            .write_table(id, &saved)
            .map_err(|e| Self::io_err(format!("failed to replay saved gamma table: {e}")))?;

        match self.api.read_table(id) {
            Ok(confirm) if confirm == saved => {
                self.holds.clear(&self.selector);
                // Task 8 crash insurance: remove this selector from the
                // breadcrumb only AFTER a confirmed, successful wake replay
                // (mirrors the hold-registry clear immediately above).
                // Best-effort: a removal failure does not un-succeed a wake
                // that has already physically landed — logged, not
                // propagated (mirrors the startup/shutdown restore paths'
                // "log, never abort" contract in `dormantd::gamma_recovery`).
                if let Some(Err(e)) = self
                    .breadcrumb
                    .as_ref()
                    .map(|bc| bc.remove_selector(&self.selector))
                {
                    tracing::warn!(
                        event = "gamma_breadcrumb_clear_failed",
                        selector = %self.selector,
                        error = %e,
                    );
                }
                Ok(())
            }
            Ok(_) => Err(Self::io_err(format!(
                "post-wake verification mismatch replaying saved table for '{}'",
                self.selector
            ))),
            Err(e) => Err(Self::io_err(format!(
                "post-wake verification read failed: {e}"
            ))),
        }
    }

    async fn read_state(&self) -> Option<PanelState> {
        let id = self.api.resolve(&self.selector).ok()?;
        let table = self.api.read_table(id).ok()?;
        let brightness = if table.is_black() { Some(0) } else { Some(100) };
        Some(PanelState {
            power: None,
            brightness,
        })
    }

    fn panel_identity(&self) -> Option<String> {
        Some(self.selector.clone())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh, temp-dir-backed [`GammaBreadcrumb`] for breadcrumb-focused
    /// tests. Returns the owning [`tempfile::TempDir`] too — it must stay
    /// alive for the breadcrumb's directory to keep existing.
    fn test_breadcrumb() -> (tempfile::TempDir, Arc<GammaBreadcrumb>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bc = Arc::new(GammaBreadcrumb::new(dir.path()));
        (dir, bc)
    }

    // ── FakeGammaApi ───────────────────────────────────────────────────────

    struct FakeInner {
        /// selector -> numeric id.
        ids: HashMap<String, CGDirectDisplayID>,
        /// numeric id -> current table (mutated by `write_table`).
        tables: HashMap<CGDirectDisplayID, GammaTable>,
        read_calls: u32,
        /// Scripted read overrides, consumed in FIFO order before falling
        /// back to the real stored table.
        read_overrides: VecDeque<Result<GammaTable, GammaError>>,
        /// When true, every `read_table` call AFTER the first `write_table`
        /// call returns an error — models a Quartz write that silently
        /// failed to land.
        confirm_always_fails_after_write: bool,
        has_written: bool,
        blank_write_calls: u32,
        /// Writes of a non-black table — a saved-table replay, whether from
        /// `wake()` or a rollback inside `blank()`.
        saved_table_replay_calls: u32,
        /// Kept for API parity with the plan's test vocabulary: this fake
        /// (and the real `GammaApi` trait) has no system-wide restore call
        /// at all, so this can never be anything but 0 — the assertions
        /// that reference it document "wake never calls a system-wide
        /// restore" structurally, not just empirically.
        restore_all_calls: u32,
    }

    #[derive(Clone)]
    struct FakeGammaApi {
        inner: Arc<StdMutex<FakeInner>>,
        next_id: Arc<AtomicU32>,
    }

    impl FakeGammaApi {
        fn empty() -> Self {
            Self {
                inner: Arc::new(StdMutex::new(FakeInner {
                    ids: HashMap::new(),
                    tables: HashMap::new(),
                    read_calls: 0,
                    read_overrides: VecDeque::new(),
                    confirm_always_fails_after_write: false,
                    has_written: false,
                    blank_write_calls: 0,
                    saved_table_replay_calls: 0,
                    restore_all_calls: 0,
                })),
                next_id: Arc::new(AtomicU32::new(1)),
            }
        }

        /// Add a display mapped to `selector`, seeded with `table`.
        fn add_display(&self, selector: &str, table: GammaTable) -> &Self {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let mut inner = self.inner.lock().unwrap();
            inner.ids.insert(selector.to_string(), id);
            inner.tables.insert(id, table);
            self
        }

        /// One display, `selector` -> `table`.
        fn with_table(selector: &str, table: GammaTable) -> Self {
            let fake = Self::empty();
            fake.add_display(selector, table);
            fake
        }

        /// Two displays, "cg:a" and "cg:b", seeded with DISTINGUISHABLE
        /// non-black linear tables (256 vs. 128 samples). Deliberately
        /// different — not just "both non-black" — so a wake that replays
        /// the wrong selector's saved table onto the right display is
        /// actually detectable by content, rather than being masked by two
        /// selectors happening to hold byte-identical tables.
        fn two_displays() -> Self {
            let fake = Self::empty();
            fake.add_display("cg:a", GammaTable::linear(256));
            fake.add_display("cg:b", GammaTable::linear(128));
            fake
        }

        /// One display whose confirmation read fails forever after the
        /// first write — models a black write that silently didn't land.
        fn write_black_then_fail_confirmation(selector: &str) -> Self {
            let fake = Self::with_table(selector, GammaTable::linear(256));
            fake.inner.lock().unwrap().confirm_always_fails_after_write = true;
            fake
        }

        /// One display seeded with a plausible non-black table — used by
        /// chain-degradation tests where the exact table shape is
        /// irrelevant, only that it isn't already black.
        fn working(selector: &str) -> Self {
            Self::with_table(selector, GammaTable::linear(256))
        }

        fn read_calls(&self) -> u32 {
            self.inner.lock().unwrap().read_calls
        }

        fn saved_table_replay_calls(&self) -> u32 {
            self.inner.lock().unwrap().saved_table_replay_calls
        }

        /// Count of `write_table` calls that wrote a BLACK table — i.e. the
        /// blank-path LUT write itself (as opposed to
        /// [`Self::saved_table_replay_calls`]'s non-black saved-table
        /// replays). Used to prove a failed breadcrumb persist aborts
        /// `blank()` before touching the LUT at all — see
        /// `blank_aborts_before_any_lut_write_when_breadcrumb_persist_fails`.
        fn blank_write_calls(&self) -> u32 {
            self.inner.lock().unwrap().blank_write_calls
        }

        fn restore_all_calls(&self) -> u32 {
            self.inner.lock().unwrap().restore_all_calls
        }

        /// Current table for `selector` — panics if the selector is
        /// unknown (a test bug, not a runtime condition).
        fn current(&self, selector: &str) -> GammaTable {
            let inner = self.inner.lock().unwrap();
            let id = inner.ids[selector];
            inner.tables[&id].clone()
        }
    }

    impl GammaApi for FakeGammaApi {
        fn resolve(&self, selector: &str) -> Result<CGDirectDisplayID, GammaError> {
            self.inner
                .lock()
                .unwrap()
                .ids
                .get(selector)
                .copied()
                .ok_or_else(|| GammaError::from(format!("no online display matches '{selector}'")))
        }

        fn read_table(&self, display: CGDirectDisplayID) -> Result<GammaTable, GammaError> {
            let mut inner = self.inner.lock().unwrap();
            inner.read_calls += 1;
            if let Some(scripted) = inner.read_overrides.pop_front() {
                return scripted;
            }
            if inner.confirm_always_fails_after_write && inner.has_written {
                return Err(GammaError::from("simulated confirmation read failure"));
            }
            inner
                .tables
                .get(&display)
                .cloned()
                .ok_or_else(|| GammaError::from(format!("no table for display id {display}")))
        }

        fn write_table(
            &self,
            display: CGDirectDisplayID,
            table: &GammaTable,
        ) -> Result<(), GammaError> {
            let mut inner = self.inner.lock().unwrap();
            if !inner.tables.contains_key(&display) {
                return Err(GammaError::from(format!(
                    "no table for display id {display}"
                )));
            }
            if table.is_black() {
                inner.blank_write_calls += 1;
            } else {
                inner.saved_table_replay_calls += 1;
            }
            inner.tables.insert(display, table.clone());
            inner.has_written = true;
            Ok(())
        }
    }

    // ── RED-first controller tests ───────────────────────────────────────

    #[tokio::test]
    async fn first_blank_wins_and_repeated_blank_never_saves_black() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            Arc::clone(&holds),
        );

        controller.blank(BlankMode::BrightnessZero).await.unwrap();
        controller.blank(BlankMode::BrightnessZero).await.unwrap();

        // Exactly 2 read_table calls total across BOTH blank() calls: the
        // first blank's pre-write capture (1) and its post-write
        // confirmation (1). The second, idempotent blank never touches the
        // API at all (first-blank-wins short-circuit) — see the module
        // doc's "First-blank-wins saved state" section.
        assert_eq!(api.read_calls(), 2);
        assert_eq!(holds.saved("cg:panel"), Some(GammaTable::linear(256)));
        assert!(api.current("cg:panel").is_black());
    }

    #[tokio::test]
    async fn unowned_already_black_table_is_never_saved() {
        let api = Arc::new(FakeGammaApi::with_table("cg:panel", GammaTable::black(256)));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller =
            MacosGammaBlackController::with_api("cg:panel".into(), api, Arc::clone(&holds));

        let err = controller
            .blank(BlankMode::BrightnessZero)
            .await
            .unwrap_err();
        assert!(
            err.error.starts_with("E_DISPLAY_IO:"),
            "error must start with E_DISPLAY_IO: {err}"
        );
        assert_eq!(holds.saved("cg:panel"), None);
    }

    #[tokio::test]
    async fn dropping_a_blanked_controller_does_not_restore_gamma() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
        );
        controller.blank(BlankMode::BrightnessZero).await.unwrap();

        drop(controller);

        assert!(api.current("cg:panel").is_black());
        assert_eq!(api.restore_all_calls(), 0);
    }

    #[tokio::test]
    async fn wake_replays_only_the_saved_table_for_this_display() {
        let api = Arc::new(FakeGammaApi::two_displays());
        // Captured before either controller blanks anything — this is
        // cg:a's own original table, distinguishable (by content, not just
        // "non-black") from cg:b's, so we can prove wake() replayed THIS
        // exact table rather than some other held selector's.
        let original_a = api.current("cg:a");
        let holds = Arc::new(GammaHoldRegistry::default());
        let a = MacosGammaBlackController::with_api(
            "cg:a".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            Arc::clone(&holds),
        );
        let b = MacosGammaBlackController::with_api(
            "cg:b".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
        );
        a.blank(BlankMode::BrightnessZero).await.unwrap();
        b.blank(BlankMode::BrightnessZero).await.unwrap();

        a.wake().await.unwrap();

        assert_eq!(
            api.current("cg:a"),
            original_a,
            "wake() must replay exactly cg:a's own saved table, not any \
             other held selector's — content must match, not merely be \
             non-black"
        );
        assert!(api.current("cg:b").is_black());
        assert_eq!(api.restore_all_calls(), 0);
    }

    #[tokio::test]
    async fn failed_blank_confirmation_replays_saved_table_before_error() {
        let api = Arc::new(FakeGammaApi::write_black_then_fail_confirmation("cg:panel"));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
        );

        assert!(controller.blank(BlankMode::BrightnessZero).await.is_err());

        assert!(!api.current("cg:panel").is_black());
        assert_eq!(api.saved_table_replay_calls(), 1);
    }

    // ── Task 8: breadcrumb persistence wiring ─────────────────────────────

    #[tokio::test]
    async fn blank_persists_breadcrumb_before_the_lut_write() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let controller = MacosGammaBlackController::with_api_and_breadcrumb(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
            Arc::clone(&breadcrumb),
        );

        assert!(!breadcrumb.exists(), "no breadcrumb before any blank");
        controller.blank(BlankMode::BrightnessZero).await.unwrap();

        let state = breadcrumb.read().expect("breadcrumb persisted by blank()");
        assert!(state.held_selectors.contains("cg:panel"));
        assert!(api.current("cg:panel").is_black());
    }

    #[tokio::test]
    async fn blank_aborts_before_any_lut_write_when_breadcrumb_persist_fails() {
        // Proves the breadcrumb-persist-before-LUT-write ordering invariant
        // (module docs, "Task 8 crash insurance" comment in `blank()`): make
        // the breadcrumb path un-writable by pre-creating
        // `gamma-blank.json` as a DIRECTORY, so `add_selector`'s atomic
        // temp-file-then-rename fails (rename onto an existing directory is
        // rejected by the OS). If `blank()` ever wrote the LUT before (or
        // without regard to) a successful breadcrumb persist, this test
        // would see a black table / nonzero write count despite the
        // breadcrumb failure — exactly what reviewer Mutation A (swapping
        // the write/persist order) would cause.
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (dir, breadcrumb) = test_breadcrumb();
        std::fs::create_dir(
            dir.path()
                .join(crate::gamma_breadcrumb::BREADCRUMB_FILENAME),
        )
        .expect("pre-create breadcrumb path as a directory");
        let controller = MacosGammaBlackController::with_api_and_breadcrumb(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
            Arc::clone(&breadcrumb),
        );

        let err = controller
            .blank(BlankMode::BrightnessZero)
            .await
            .unwrap_err();

        assert!(
            err.error
                .contains("failed to persist gamma-blank breadcrumb before write"),
            "error must name the breadcrumb persist failure: {err}"
        );
        assert_eq!(
            api.blank_write_calls(),
            0,
            "a failed breadcrumb persist must abort blank() before ANY LUT \
             write lands — Mutation A (swapping write/persist order) makes \
             this assertion fail"
        );
        assert!(
            !api.current("cg:panel").is_black(),
            "the display's table must be untouched when breadcrumb persist \
             fails"
        );
    }

    #[tokio::test]
    async fn wake_clears_breadcrumb_after_confirmed_replay() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let controller = MacosGammaBlackController::with_api_and_breadcrumb(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
            Arc::clone(&breadcrumb),
        );

        controller.blank(BlankMode::BrightnessZero).await.unwrap();
        assert!(breadcrumb.exists(), "breadcrumb held after blank");

        controller.wake().await.unwrap();
        assert!(
            !breadcrumb.exists(),
            "breadcrumb must be cleared after a confirmed wake replay"
        );
    }

    #[tokio::test]
    async fn breadcrumb_survives_repeated_idempotent_blank() {
        // First-blank-wins: the second blank() call is a no-op short-circuit
        // (see `first_blank_wins_and_repeated_blank_never_saves_black`) and
        // must not re-add (or otherwise disturb) the already-held selector.
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let controller = MacosGammaBlackController::with_api_and_breadcrumb(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
            Arc::clone(&breadcrumb),
        );

        controller.blank(BlankMode::BrightnessZero).await.unwrap();
        controller.blank(BlankMode::BrightnessZero).await.unwrap();

        let state = breadcrumb.read().expect("breadcrumb still present");
        assert_eq!(state.held_selectors.len(), 1);
    }

    #[tokio::test]
    async fn with_api_without_breadcrumb_still_blanks_and_wakes() {
        // Most of this module's tests use `with_api` (no breadcrumb) — this
        // pins that the breadcrumb wiring is genuinely optional and never
        // required for the core blank/wake contract to keep working.
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
        );

        controller.blank(BlankMode::BrightnessZero).await.unwrap();
        assert!(api.current("cg:panel").is_black());
        controller.wake().await.unwrap();
        assert!(!api.current("cg:panel").is_black());
    }

    // ── Additional coverage (not in the plan's RED list, cheap + honest) ──

    #[tokio::test]
    async fn blank_rejects_unsupported_mode() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api("cg:panel".into(), api, holds);

        let err = controller.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(err.error.starts_with("E_DISPLAY_IO:"));
        assert!(err.error.contains("unsupported blank mode"));
    }

    #[tokio::test]
    async fn wake_without_prior_blank_is_a_noop() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
        );

        controller.wake().await.unwrap();

        // Never blanked — the table this fake started with is untouched.
        assert!(!api.current("cg:panel").is_black());
    }

    #[tokio::test]
    async fn read_state_reports_zero_only_when_fully_black() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
        );

        let before = controller.read_state().await.unwrap();
        assert_eq!(before.brightness, Some(100));

        controller.blank(BlankMode::BrightnessZero).await.unwrap();

        let after = controller.read_state().await.unwrap();
        assert_eq!(after.brightness, Some(0));
    }

    #[tokio::test]
    async fn panel_identity_is_the_selector() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api("cg:panel".into(), api, holds);
        assert_eq!(controller.panel_identity(), Some("cg:panel".to_string()));
    }

    #[tokio::test]
    async fn supported_modes_is_brightness_zero_only() {
        let api = Arc::new(FakeGammaApi::with_table(
            "cg:panel",
            GammaTable::linear(256),
        ));
        let holds = Arc::new(GammaHoldRegistry::default());
        let controller = MacosGammaBlackController::with_api("cg:panel".into(), api, holds);
        assert_eq!(
            controller.supported_modes(),
            vec![BlankMode::BrightnessZero]
        );
        assert_eq!(controller.name(), "macos-gamma-black");
    }

    // ── GammaHoldRegistry: independent per-selector locking ───────────────

    #[test]
    fn holds_for_different_selectors_do_not_share_state() {
        let holds = GammaHoldRegistry::default();
        assert_eq!(holds.saved("cg:a"), None);
        assert_eq!(holds.saved("cg:b"), None);

        holds.set("cg:a", GammaTable::linear(4));
        assert_eq!(holds.saved("cg:a"), Some(GammaTable::linear(4)));
        assert_eq!(
            holds.saved("cg:b"),
            None,
            "setting one selector's hold must not affect another's"
        );

        holds.clear("cg:a");
        assert_eq!(holds.saved("cg:a"), None);
    }

    #[test]
    fn unrelated_selector_holds_use_independent_locks() {
        // Each selector's slot is its own `Arc<Mutex<..>>` — holding "cg:a"'s
        // lock (simulated here by taking the slot directly) must not block
        // a concurrent access to "cg:b"'s slot.
        let holds = GammaHoldRegistry::default();
        let slot_a = holds.slot("cg:a");
        let _guard = slot_a.lock().unwrap();

        // "cg:b" must still be freely accessible while "cg:a" is locked.
        assert_eq!(holds.saved("cg:b"), None);
    }

    // ── GammaTable ─────────────────────────────────────────────────────────

    #[test]
    fn gamma_table_black_is_black() {
        assert!(GammaTable::black(16).is_black());
    }

    #[test]
    fn gamma_table_linear_is_not_black() {
        assert!(!GammaTable::linear(16).is_black());
    }

    #[test]
    fn gamma_table_validate_rejects_empty() {
        let t = GammaTable {
            red: vec![],
            green: vec![],
            blue: vec![],
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn gamma_table_validate_rejects_mismatched_lengths() {
        let t = GammaTable {
            red: vec![0.0, 1.0],
            green: vec![0.0],
            blue: vec![0.0, 1.0],
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn gamma_table_validate_rejects_non_finite() {
        let t = GammaTable {
            red: vec![f32::NAN],
            green: vec![0.0],
            blue: vec![0.0],
        };
        assert!(t.validate().is_err());

        let t = GammaTable {
            red: vec![f32::INFINITY],
            green: vec![0.0],
            blue: vec![0.0],
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn gamma_table_validate_accepts_well_formed() {
        assert!(GammaTable::linear(8).validate().is_ok());
        assert!(GammaTable::black(8).validate().is_ok());
    }

    // ── Chain-degradation: the practical stand-in for the plan's
    //    AssemblyHarness-based test (see report for the full rationale).
    //
    // `MacosGammaBlackController` only registers on macOS
    // (`crate::registry::CONTROLLER_TYPES`), so a genuine end-to-end
    // assembly test through `dormant_core::config` + `crate::registry` can
    // only run on macOS (DEFERRED: PR CI). This test exercises the same
    // load-bearing mechanism `dormantd::app::assemble_static` relies on —
    // `DisplayExecutor` skipping an unavailable controller and falling
    // through to the next eligible one in the chain — directly, on any
    // host, by constructing a `DisplayExecutor` with an always-unavailable
    // stand-in for a degraded `ddcci` ahead of a working
    // `MacosGammaBlackController`. ──────────────────────────────────────

    /// Minimal always-unavailable controller — stands in for `ddcci` after
    /// `RealVcp` fails to resolve the private `CoreDisplay` symbols (the
    /// scenario the plan's `missing_core_display_symbols_degrade_to_gamma_without_rejecting_assembly`
    /// RED test names).
    struct AlwaysUnavailableController;

    #[async_trait]
    impl DisplayController for AlwaysUnavailableController {
        fn name(&self) -> &'static str {
            "ddcci"
        }
        fn supported_modes(&self) -> Vec<BlankMode> {
            vec![BlankMode::BrightnessZero, BlankMode::PowerOff]
        }
        async fn is_available(&self) -> bool {
            false
        }
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            Err(CmdFailure {
                controller: "ddcci".into(),
                error: format!("{E_DISPLAY_IO}: unreachable in this test"),
            })
        }
        async fn wake(&self) -> Result<(), CmdFailure> {
            Err(CmdFailure {
                controller: "ddcci".into(),
                error: format!("{E_DISPLAY_IO}: unreachable in this test"),
            })
        }
    }

    #[tokio::test]
    async fn unavailable_ddcci_degrades_to_gamma_black_in_the_chain() {
        use crate::executor::{DisplayExecutor, RetrySettings};
        use dormant_core::traits::CommandSink;
        use dormant_core::types::DisplayId;

        let api = Arc::new(FakeGammaApi::working("cg:panel"));
        let holds = Arc::new(GammaHoldRegistry::default());
        let gamma = MacosGammaBlackController::with_api(
            "cg:panel".into(),
            Arc::clone(&api) as Arc<dyn GammaApi>,
            holds,
        );

        let chain: Vec<Box<dyn DisplayController>> =
            vec![Box::new(AlwaysUnavailableController), Box::new(gamma)];

        // The chain's union of static capabilities still includes
        // BrightnessZero (gamma advertises it even though ddcci is
        // unavailable) — this is exactly what `assemble_static` checks
        // before ever issuing a command, proving assembly stays valid.
        let executor = DisplayExecutor::new(
            DisplayId("panel".into()),
            chain,
            BlankMode::BrightnessZero,
            RetrySettings {
                wake_retries: 0,
                wake_retry_backoff: std::time::Duration::from_millis(1),
            },
        );
        assert!(
            executor
                .effective_modes()
                .contains(&BlankMode::BrightnessZero),
            "chain must still advertise BrightnessZero via the gamma fallback"
        );

        // blank()/wake() must route through gamma — ddcci is skipped
        // because `is_available()` is false, never because it was removed
        // from the chain.
        executor.blank(BlankMode::BrightnessZero).await.unwrap();
        assert!(api.current("cg:panel").is_black());

        executor.wake().await.unwrap();
        assert!(!api.current("cg:panel").is_black());
    }
}
