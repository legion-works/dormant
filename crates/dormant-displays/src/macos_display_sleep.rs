//! `macos-display-sleep` display controller — a **global** macOS fallback
//! that puts every attached display to sleep via `pmset displaysleepnow` and
//! confirms wake by declaring a short-lived local user-activity IOPM
//! assertion and polling CoreGraphics per-display sleep state until every
//! *online* display reports awake.
//!
//! ## Why global, not per-display (Task 10, ratified — see
//! `docs/research/2026-07-16-macos-display-selector.md`)
//!
//! Unlike [`crate::macos_gamma_black::MacosGammaBlackController`] (which is
//! addressed by a stable per-panel `cg:<uuid>` selector and refuses to build
//! without one), this controller has **no** selector concept at all —
//! `pmset displaysleepnow` and the IOPM user-activity assertion are both
//! system-wide primitives with no per-display targeting surface. It is the
//! last-resort fallback for a macOS host with no DDC/CI, no Quartz gamma
//! table (headless/virtual display), or where those have already degraded —
//! blanking it puts the ENTIRE Mac's display(s) to sleep, not just the one
//! display config entry that requested it. Config validation
//! (`dormant_core::config::validate`) enforces the corresponding contract:
//! `output` must be absent or the literal string `"all"` — any *named*
//! selector (`"cg:..."`, `"DP-1"`, etc.) is a hard error, because this
//! controller could never honor a per-display target.
//!
//! ## Blank — one bounded subprocess call
//!
//! `blank()` spawns `/usr/bin/pmset displaysleepnow` via
//! [`PMSET_SLEEPNOW_ARGS`] (a literal argument array — **never** a shell
//! string) and bounds the whole call with `command_timeout`. Exactly one
//! attempt per `blank()` call — retries and fallback-chain escalation are
//! the executor's job (`crate::executor::DisplayExecutor`), not this
//! controller's (mirrors every other controller in this crate).
//!
//! ## Wake — declare, poll, confirm (the readback is the verdict)
//!
//! `wake()`:
//!
//! 1. Declares a short-lived local user-activity IOPM assertion
//!    (`IOPMAssertionDeclareUserActivity`) via
//!    [`DisplaySleepTransport::declare_user_activity`], which returns an
//!    [`AssertionGuard`] — releasing the assertion is the guard's `Drop`,
//!    not a separate call this controller makes explicitly (see "RAII
//!    design" below).
//! 2. Polls [`DisplaySleepTransport::online_sleep_states`] every
//!    `POLL_INTERVAL` until **every** online display reports awake, bounded
//!    by `command_timeout` overall.
//! 3. Succeeds ONLY when step 2's readback confirms every online display
//!    awake — a successful process spawn or a successful assertion
//!    declaration is never, on its own, wake success. This mirrors
//!    `MacosGammaBlackController::wake`'s "the post-write readback is the
//!    verdict" contract, just applied to a coarser (system-wide, not
//!    per-panel) signal.
//!
//! ## RAII design — the guard IS the release path
//!
//! [`AssertionGuard`] is owned by a `let` binding in `wake()`'s own async fn
//! body — not spawned onto a detached task, not stored in `self`. Rust drops
//! every local variable of a future when that future is dropped, on ANY
//! exit path:
//!
//! - normal return (`Ok` after a confirmed readback, or `Err` after a failed
//!   readback or exhausted `command_timeout`) — the guard drops when
//!   `wake()`'s `async fn` body finishes executing, releasing the assertion;
//! - the executor's supersede mechanism cancels an in-flight wake by
//!   dropping its `JoinHandle`/future outright (see
//!   `crate::executor::DisplayExecutor`'s cancellation-token model) — the
//!   guard, as a live local in the dropped future, is dropped and releases
//!   the assertion at whatever `.await` point the drop lands on, with no
//!   extra code needed here;
//! - a caller `abort()`s the task the future is running on — identical to
//!   the point above; Rust's drop glue runs regardless of *why* the future
//!   was dropped.
//!
//! There is deliberately **no** persistent, daemon-lifetime assertion
//! outside an in-flight `wake()` call — an assertion that outlived its wake
//! attempt would silently keep the Mac awake forever after a single wake,
//! defeating the whole point of the sleep controller.
//!
//! ## Real backend
//!
//! The real, macOS-only IOPM/CoreGraphics FFI backend
//! (`crate::macos_power::RealDisplaySleepTransport`) is a separate, thin,
//! `#[cfg(target_os = "macos")]`-gated module — same split as
//! [`crate::macos_gamma_black`] (platform-neutral controller logic, tested
//! here via `FakeDisplaySleepTransport`) / `crate::macos_display_catalog`
//! (thin real FFI backend). DEFERRED: PR CI — `crate::macos_power` cannot
//! compile or run in this Linux sandbox (nor, being `#[cfg(target_os =
//! "macos")]`-gated at its `mod` declaration, can this doc link to it on
//! any other target).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

pub use crate::macos_gamma_black::CGDirectDisplayID;

/// Literal controller name — grep-stable, matches the `macos-display-sleep`
/// config `type`.
const NAME: &str = "macos-display-sleep";

/// Interval between successive [`DisplaySleepTransport::online_sleep_states`]
/// polls during [`MacosDisplaySleepController::wake`]'s confirmation loop.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The exact `pmset` argument array `crate::macos_power::RealDisplaySleepTransport`
/// spawns on every `blank()` call — a literal array, **never** a shell
/// string (Task 10 invariant). Public so this Linux-runnable module can pin
/// the exact array without needing to run the macOS-only real transport.
pub const PMSET_SLEEPNOW_ARGS: &[&str] = &["displaysleepnow"];

// ── AssertionGuard ───────────────────────────────────────────────────────

/// RAII guard for a declared IOPM user-activity assertion.
///
/// Holding this value keeps the assertion alive; dropping it — on normal
/// return, error, or because the future that owns it was cancelled/aborted
/// — releases the assertion exactly once. See the module docs' "RAII
/// design" section for why this is the ENTIRE release mechanism (no
/// separate explicit-release call anywhere in this controller).
pub struct AssertionGuard {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl AssertionGuard {
    /// Build a guard whose `release` callback runs at most once, on drop.
    ///
    /// `release` is the one and only place a [`DisplaySleepTransport`]
    /// implementation's "free this assertion" logic lives — the real
    /// backend closes over the numeric `IOPMAssertionID` and calls
    /// `IOPMAssertionRelease`; this module's own test-only
    /// `FakeDisplaySleepTransport` closes over its own call-counting state.
    #[must_use]
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl std::fmt::Debug for AssertionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssertionGuard").finish_non_exhaustive()
    }
}

impl Drop for AssertionGuard {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

// ── DisplaySleepTransport ─────────────────────────────────────────────────

/// Abstraction over the three macOS primitives this controller needs — real
/// (IOKit/CoreGraphics FFI, `crate::macos_power`) or fake (this module's
/// tests). Only [`Self::sleep_all`] is genuinely asynchronous (it spawns and
/// waits on a subprocess); the other two are cheap, local, synchronous
/// calls — the same "no `spawn_blocking` needed" reasoning
/// [`crate::macos_gamma_black::GammaApi`]'s docs give for the Quartz gamma
/// calls applies here too (in-process IOKit/CoreGraphics reads, no bus I/O).
#[async_trait]
pub trait DisplaySleepTransport: Send + Sync {
    /// Run `pmset displaysleepnow` (via [`PMSET_SLEEPNOW_ARGS`]) to put
    /// every display to sleep.
    ///
    /// # Errors
    ///
    /// Returns a [`CmdFailure`] on spawn failure, non-zero exit, or (real
    /// backend only) a caller-applied timeout.
    async fn sleep_all(&self) -> Result<(), CmdFailure>;

    /// Declare a short-lived local user-activity IOPM assertion, returning a
    /// guard whose `Drop` releases it (see [`AssertionGuard`]).
    ///
    /// # Errors
    ///
    /// Returns a [`CmdFailure`] if the assertion could not be declared.
    fn declare_user_activity(&self) -> Result<AssertionGuard, CmdFailure>;

    /// Read the sleep state of every *online* display: `(id, asleep)` pairs.
    ///
    /// # Errors
    ///
    /// Returns a [`CmdFailure`] on an enumeration/readback failure.
    fn online_sleep_states(&self) -> Result<Vec<(CGDirectDisplayID, bool)>, CmdFailure>;
}

// ── MacosDisplaySleepController ─────────────────────────────────────────

/// Global macOS display-sleep fallback controller. See the module docs.
///
/// Capability: [`BlankMode::PowerOff`] only.
pub struct MacosDisplaySleepController {
    command_timeout: Duration,
    transport: Arc<dyn DisplaySleepTransport>,
}

impl MacosDisplaySleepController {
    /// Build a controller with the real IOKit/CoreGraphics/`pmset` backend
    /// (`crate::macos_power::RealDisplaySleepTransport`). Only available on
    /// macOS.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn new(command_timeout: Duration) -> Self {
        Self::with_transport(
            command_timeout,
            Arc::new(crate::macos_power::RealDisplaySleepTransport),
        )
    }

    /// Build a controller with a custom [`DisplaySleepTransport`] (used by
    /// this module's own tests to inject its test-only
    /// `FakeDisplaySleepTransport`).
    #[must_use]
    pub fn with_transport(
        command_timeout: Duration,
        transport: Arc<dyn DisplaySleepTransport>,
    ) -> Self {
        Self {
            command_timeout,
            transport,
        }
    }

    /// Build a [`CmdFailure`] with the `E_DISPLAY_IO:` prefix and this
    /// controller's name — the single formatting call site for every
    /// failure below (mirrors `MacosGammaBlackController::io_err`).
    fn io_err(detail: impl std::fmt::Display) -> CmdFailure {
        CmdFailure {
            controller: NAME.to_string(),
            error: format!("{E_DISPLAY_IO}: {detail}"),
        }
    }
}

#[async_trait]
impl DisplayController for MacosDisplaySleepController {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        vec![BlankMode::PowerOff]
    }

    async fn is_available(&self) -> bool {
        // No per-display resolution to check (this controller is global) —
        // a missing/broken `pmset` surfaces as a spawn failure at first use,
        // exactly like `CommandController::is_available`'s reasoning.
        true
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        if mode != BlankMode::PowerOff {
            return Err(Self::io_err(format!("unsupported blank mode {mode:?}")));
        }

        // ONE bounded attempt — the executor owns retries/escalation.
        match tokio::time::timeout(self.command_timeout, self.transport.sleep_all()).await {
            Ok(result) => result,
            Err(_elapsed) => Err(Self::io_err(format!(
                "timeout after {:?} running pmset {}",
                self.command_timeout,
                PMSET_SLEEPNOW_ARGS.join(" "),
            ))),
        }
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        // RAII: `_guard` is a local of THIS async fn's own stack frame — see
        // the module docs' "RAII design" section. It is dropped (releasing
        // the assertion) on every exit path below, AND on whatever
        // `.await` point a cancelled/aborted caller drops this future at,
        // with no extra code required here.
        let _guard = self
            .transport
            .declare_user_activity()
            .map_err(|e| Self::io_err(format!("failed to declare user activity: {e}")))?;

        // Cooperative yield before any readback I/O begins: gives an
        // already-superseded/aborted wake a checkpoint to unwind (releasing
        // `_guard`) without ever touching the transport again.
        tokio::task::yield_now().await;

        match tokio::time::timeout(self.command_timeout, self.poll_until_all_awake()).await {
            Ok(result) => result,
            Err(_elapsed) => Err(Self::io_err(format!(
                "timeout after {:?} waiting for every online display to confirm awake",
                self.command_timeout
            ))),
        }
    }
}

impl MacosDisplaySleepController {
    /// Poll [`DisplaySleepTransport::online_sleep_states`] every
    /// [`POLL_INTERVAL`] until every *online* display reports awake, or a
    /// readback itself errors. The readback is the verdict — see the module
    /// docs.
    async fn poll_until_all_awake(&self) -> Result<(), CmdFailure> {
        loop {
            let states = self
                .transport
                .online_sleep_states()
                .map_err(|e| Self::io_err(format!("failed to read display sleep state: {e}")))?;
            if states.iter().all(|&(_, asleep)| !asleep) {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    // ── FakeDisplaySleepTransport ────────────────────────────────────────

    struct FakeInner {
        sleep_all_calls: u32,
        sleep_all_result: Result<(), CmdFailure>,
        /// Optional delay applied before `sleep_all` returns its scripted
        /// result — used by the blank-timeout test.
        sleep_all_delay: Duration,
        declare_calls: u32,
        /// `Some(e)` makes every `declare_user_activity` call fail with `e`.
        declare_fails: Option<CmdFailure>,
        release_calls: u32,
        /// Net outstanding assertions: +1 on a successful declare, -1 on
        /// every guard release. Must return to 0 once every guard this test
        /// obtained has been dropped.
        live_assertions: i64,
        state_reads: u32,
        /// Scripted reads, consumed in FIFO order.
        state_script: VecDeque<Vec<(CGDirectDisplayID, bool)>>,
        /// Returned once `state_script` is exhausted — repeats forever, so
        /// an "always asleep" or "always awake" fixture never needs to
        /// script an unbounded queue.
        state_default: Vec<(CGDirectDisplayID, bool)>,
    }

    #[derive(Clone)]
    struct FakeDisplaySleepTransport {
        inner: Arc<StdMutex<FakeInner>>,
    }

    impl FakeDisplaySleepTransport {
        /// A transport that is immediately available and reports one online
        /// display, awake, by default.
        fn new() -> Self {
            Self {
                inner: Arc::new(StdMutex::new(FakeInner {
                    sleep_all_calls: 0,
                    sleep_all_result: Ok(()),
                    sleep_all_delay: Duration::ZERO,
                    declare_calls: 0,
                    declare_fails: None,
                    release_calls: 0,
                    live_assertions: 0,
                    state_reads: 0,
                    state_script: VecDeque::new(),
                    state_default: vec![(1, false)], // one online display, awake
                })),
            }
        }

        fn set_sleep_all_result(&self, result: Result<(), CmdFailure>) {
            self.inner.lock().unwrap().sleep_all_result = result;
        }

        fn set_sleep_all_delay(&self, d: Duration) {
            self.inner.lock().unwrap().sleep_all_delay = d;
        }

        fn set_declare_fails(&self, e: CmdFailure) {
            self.inner.lock().unwrap().declare_fails = Some(e);
        }

        /// Script the exact sequence of `online_sleep_states` results —
        /// consumed FIFO, one script entry per call.
        fn script_state_reads(&self, reads: Vec<Vec<(CGDirectDisplayID, bool)>>) {
            self.inner.lock().unwrap().state_script = reads.into_iter().collect();
        }

        /// Set the repeating default read (used once `state_script` is
        /// exhausted, or immediately if never scripted).
        fn set_default_state(&self, state: Vec<(CGDirectDisplayID, bool)>) {
            self.inner.lock().unwrap().state_default = state;
        }

        fn sleep_all_calls(&self) -> u32 {
            self.inner.lock().unwrap().sleep_all_calls
        }

        fn declare_calls(&self) -> u32 {
            self.inner.lock().unwrap().declare_calls
        }

        fn release_calls(&self) -> u32 {
            self.inner.lock().unwrap().release_calls
        }

        fn live_assertions(&self) -> i64 {
            self.inner.lock().unwrap().live_assertions
        }

        fn state_reads(&self) -> u32 {
            self.inner.lock().unwrap().state_reads
        }
    }

    #[async_trait]
    impl DisplaySleepTransport for FakeDisplaySleepTransport {
        async fn sleep_all(&self) -> Result<(), CmdFailure> {
            let (delay, result) = {
                let mut g = self.inner.lock().unwrap();
                g.sleep_all_calls += 1;
                (g.sleep_all_delay, g.sleep_all_result.clone())
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            result
        }

        fn declare_user_activity(&self) -> Result<AssertionGuard, CmdFailure> {
            let mut g = self.inner.lock().unwrap();
            g.declare_calls += 1;
            if let Some(e) = g.declare_fails.clone() {
                return Err(e);
            }
            g.live_assertions += 1;
            drop(g);

            let inner = Arc::clone(&self.inner);
            Ok(AssertionGuard::new(move || {
                let mut g = inner.lock().unwrap();
                g.release_calls += 1;
                g.live_assertions -= 1;
            }))
        }

        fn online_sleep_states(&self) -> Result<Vec<(CGDirectDisplayID, bool)>, CmdFailure> {
            let mut g = self.inner.lock().unwrap();
            g.state_reads += 1;
            if let Some(scripted) = g.state_script.pop_front() {
                Ok(scripted)
            } else {
                Ok(g.state_default.clone())
            }
        }
    }

    fn make_controller(
        timeout: Duration,
    ) -> (MacosDisplaySleepController, Arc<FakeDisplaySleepTransport>) {
        let fake = Arc::new(FakeDisplaySleepTransport::new());
        let ctrl = MacosDisplaySleepController::with_transport(
            timeout,
            Arc::clone(&fake) as Arc<dyn DisplaySleepTransport>,
        );
        (ctrl, fake)
    }

    fn io_err(msg: &str) -> CmdFailure {
        CmdFailure {
            controller: NAME.to_string(),
            error: format!("{E_DISPLAY_IO}: {msg}"),
        }
    }

    // ── name / supported_modes / is_available ──────────────────────────────

    #[test]
    fn name_and_supported_modes() {
        let (ctrl, _fake) = make_controller(Duration::from_secs(5));
        assert_eq!(ctrl.name(), "macos-display-sleep");
        assert_eq!(ctrl.supported_modes(), vec![BlankMode::PowerOff]);
    }

    #[tokio::test]
    async fn is_available_is_always_true() {
        let (ctrl, _fake) = make_controller(Duration::from_secs(5));
        assert!(ctrl.is_available().await);
    }

    // ── PMSET_SLEEPNOW_ARGS pin ──────────────────────────────────────────

    #[test]
    fn pmset_sleepnow_args_is_exactly_displaysleepnow() {
        assert_eq!(PMSET_SLEEPNOW_ARGS, &["displaysleepnow"]);
    }

    // ── blank() ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn blank_rejects_unsupported_mode() {
        let (ctrl, fake) = make_controller(Duration::from_secs(5));
        let err = ctrl.blank(BlankMode::BrightnessZero).await.unwrap_err();
        assert!(
            err.error.starts_with("E_DISPLAY_IO:"),
            "error must start with E_DISPLAY_IO: {err}"
        );
        assert!(err.error.contains("unsupported blank mode"));
        assert_eq!(
            fake.sleep_all_calls(),
            0,
            "an unsupported mode must never reach the transport"
        );
    }

    #[tokio::test]
    async fn blank_power_off_calls_sleep_all_exactly_once() {
        let (ctrl, fake) = make_controller(Duration::from_secs(5));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            fake.sleep_all_calls(),
            1,
            "one bounded attempt per blank() call — retries are the executor's job"
        );
    }

    #[tokio::test]
    async fn blank_propagates_transport_error() {
        let (ctrl, fake) = make_controller(Duration::from_secs(5));
        fake.set_sleep_all_result(Err(io_err("simulated pmset failure")));
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(err.error.contains("simulated pmset failure"));
    }

    #[tokio::test(start_paused = true)]
    async fn blank_timeout_returns_error_not_hang() {
        let (ctrl, fake) = make_controller(Duration::from_millis(200));
        fake.set_sleep_all_delay(Duration::from_secs(60));

        let start = tokio::time::Instant::now();
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "blank must be bounded by command_timeout; took {elapsed:?}"
        );
        assert!(err.error.starts_with("E_DISPLAY_IO:"));
        assert!(err.error.contains("timeout"));
    }

    // ── wake(): RED test 1 — every online display must confirm awake ───────

    #[tokio::test(start_paused = true)]
    async fn wake_requires_every_online_display_to_confirm_awake() {
        let (ctrl, fake) = make_controller(Duration::from_secs(5));
        // Polarity: `false` = awake, `true` = asleep (matches the tuple
        // shape `(id, asleep)` used throughout this module).
        fake.script_state_reads(vec![
            vec![(1, true), (2, true)],   // round 1: both asleep
            vec![(1, false), (2, true)],  // round 2: one awake, one asleep
            vec![(1, false), (2, false)], // round 3: all awake -> success
        ]);

        ctrl.wake().await.unwrap();

        assert_eq!(fake.declare_calls(), 1);
        assert_eq!(fake.release_calls(), 1);
        assert_eq!(fake.state_reads(), 3);
    }

    // ── wake(): RED test 2 — declare/spawn success alone is not wake success ──

    #[tokio::test(start_paused = true)]
    async fn assertion_success_without_awake_readback_is_failure() {
        let (ctrl, fake) = make_controller(Duration::from_millis(250));
        fake.set_default_state(vec![(1, true)]); // always asleep

        let err = ctrl.wake().await.unwrap_err();

        assert!(
            err.error.starts_with("E_DISPLAY_IO:"),
            "error must start with E_DISPLAY_IO: {err}"
        );
        assert_eq!(
            fake.release_calls(),
            1,
            "the assertion must still be released"
        );
        assert_eq!(fake.live_assertions(), 0);
    }

    // ── wake(): RED tests 3/4 — RAII release on dropped/aborted future ─────

    #[tokio::test(start_paused = true)]
    async fn dropping_wake_future_after_declaration_releases_assertion() {
        let (ctrl, fake) = make_controller(Duration::from_secs(5));
        fake.set_default_state(vec![(1, true)]); // always asleep -> wake would never finish on its own
        let ctrl = Arc::new(ctrl);

        let ctrl_for_wake = Arc::clone(&ctrl);
        let task = tokio::spawn(async move { ctrl_for_wake.wake().await });

        // Let the spawned task run until it has declared the assertion.
        for _ in 0..32 {
            if fake.declare_calls() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.declare_calls(), 1, "assertion must have been declared");

        task.abort();
        let _ = task.await;

        assert_eq!(
            fake.release_calls(),
            1,
            "aborting must release the assertion"
        );
        assert_eq!(fake.live_assertions(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_wake_future_during_state_readback_releases_assertion() {
        let (ctrl, fake) = make_controller(Duration::from_secs(5));
        fake.set_default_state(vec![(1, true)]); // always asleep -> loop never exits on its own
        let ctrl = Arc::new(ctrl);

        let ctrl_for_wake = Arc::clone(&ctrl);
        let task = tokio::spawn(async move { ctrl_for_wake.wake().await });

        // Let the spawned task run until it has performed at least one
        // state readback (strictly later than the "after declaration"
        // checkpoint above).
        for _ in 0..32 {
            if fake.state_reads() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            fake.state_reads() >= 1,
            "must reach at least one state readback before aborting"
        );

        task.abort();
        let _ = task.await;

        assert_eq!(
            fake.release_calls(),
            1,
            "aborting must release the assertion"
        );
        assert_eq!(fake.live_assertions(), 0);
    }

    #[tokio::test]
    async fn wake_declare_failure_never_reaches_readback() {
        let (ctrl, fake) = make_controller(Duration::from_secs(5));
        fake.set_declare_fails(io_err("simulated IOPM declare failure"));

        let err = ctrl.wake().await.unwrap_err();
        assert!(err.error.starts_with("E_DISPLAY_IO:"));
        assert_eq!(
            fake.state_reads(),
            0,
            "must never poll state without a live assertion"
        );
        assert_eq!(
            fake.release_calls(),
            0,
            "nothing to release — declare itself failed"
        );
    }
}
