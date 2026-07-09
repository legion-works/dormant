//! In-crate test doubles for the [`crate::traits`] surfaces — sensor sources
//! and command sinks.  Compiled only under the `test-fakes` feature so they
//! stay out of release binaries.
//!
//! ## Why `Vec` and not `VecDeque`
//!
//! Both helpers expose a `Clone`-able handle that returns owned data on
//! demand, so we pay for the clone / lock overhead only when the test asks.

#![cfg(feature = "test-fakes")]
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::traits::{CommandSink, DisplayController, PanelState, RenderSink, SensorSource};
use crate::types::{BlankMode, CmdFailure, PresenceEvent, StageKind};

// ── FakeSensorSource ───────────────────────────────────────────────────────────

/// A scripted [`SensorSource`] — replays a sequence of `(virtual_offset,
/// PresenceEvent)` entries using `tokio::time::sleep` so paused tests can
/// advance virtual time arbitrarily.
///
/// The source sleeps the *delta* between consecutive entries, not the absolute
/// offset, so adding/removing events does not require rewriting later offsets.
///
/// On script exhaustion the source exits without emitting a final
/// `Unavailable` — that policy is the engine's call, not the sensor's.
#[derive(Debug, Clone)]
pub struct FakeSensorSource {
    /// Source id reported by `source_id`.
    pub id: String,
    /// Scripted events: `(delta_from_previous_entry, event)`.
    pub script: Vec<(Duration, PresenceEvent)>,
}

#[async_trait]
impl SensorSource for FakeSensorSource {
    fn source_id(&self) -> &str {
        &self.id
    }

    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<PresenceEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let iter = self.script.into_iter();
        // First entry is the time delta from "now"; no preceding reference.
        for (delta, event) in iter {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                () = tokio::time::sleep(delta) => {}
            }
            // If the receiver has gone away, exit quietly.
            if tx.send(event).await.is_err() {
                return Ok(());
            }
        }
        // Script exhausted — keep the task alive until cancelled so the
        // engine does not see a premature channel-close.
        cancel.cancelled().await;
        Ok(())
    }
}

// ── RecordingSink ─────────────────────────────────────────────────────────────

/// A single blank/wake call recorded by [`RecordingSink`].
#[derive(Debug, Clone, PartialEq)]
pub enum SinkCmd {
    /// A blank command (with the requested mode).
    Blank(BlankMode),
    /// A wake command.
    Wake,
}

/// A [`CommandSink`] that records every call (with the virtual time at which
/// it was made, measured from sink creation via `tokio::time::Instant::now`)
/// and serves scripted results for blank and wake independently.
///
/// Empty result queues mean "default Ok".  Use [`Self::push_blank_result`] /
/// [`Self::push_wake_result`] to enqueue scripted failures.
#[derive(Debug, Clone)]
pub struct RecordingSink {
    /// Shared log + result queues — `Arc<Mutex<...>>` so the public API can
    /// hand out snapshots without disturbing the running engine.
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    /// `(virtual_offset_from_creation, command)` for every issued call.
    log: Vec<(Duration, SinkCmd)>,
    /// Scripted blank results — popped FIFO; empty means Ok.
    blank_results: VecDeque<Result<(), CmdFailure>>,
    /// Scripted wake results — popped FIFO; empty means Ok.
    wake_results: VecDeque<Result<(), CmdFailure>>,
    /// Monotonic instant captured at construction for virtual timestamps.
    created_at: tokio::time::Instant,
    /// Scripted controller health snapshot (default empty).
    health: Vec<crate::rules::ControllerHealth>,
}

impl RecordingSink {
    /// Create a fresh sink.  Use [`Clone`] to share between the engine and the
    /// test harness.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                log: Vec::new(),
                blank_results: VecDeque::new(),
                wake_results: VecDeque::new(),
                created_at: tokio::time::Instant::now(),
                health: Vec::new(),
            })),
        }
    }

    /// Snapshot of every command issued so far, oldest first.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (another task panicked while
    /// holding the lock).
    #[must_use]
    pub fn log(&self) -> Vec<(Duration, SinkCmd)> {
        self.inner
            .lock()
            .expect("RecordingSink lock poisoned")
            .log
            .clone()
    }

    /// Push a scripted blank result.  Drained FIFO by [`CommandSink::blank`].
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (another task panicked while
    /// holding the lock).
    pub fn push_blank_result(&self, result: Result<(), CmdFailure>) {
        self.inner
            .lock()
            .expect("RecordingSink lock poisoned")
            .blank_results
            .push_back(result);
    }

    /// Push a scripted wake result.  Drained FIFO by [`CommandSink::wake`].
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (another task panicked while
    /// holding the lock).
    pub fn push_wake_result(&self, result: Result<(), CmdFailure>) {
        self.inner
            .lock()
            .expect("RecordingSink lock poisoned")
            .wake_results
            .push_back(result);
    }

    /// Set the controller health snapshot returned by
    /// [`CommandSink::controller_health`].  Default is empty.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (another task panicked while
    /// holding the lock).
    pub fn set_health(&self, health: Vec<crate::rules::ControllerHealth>) {
        self.inner
            .lock()
            .expect("RecordingSink lock poisoned")
            .health = health;
    }
}

impl Default for RecordingSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CommandSink for RecordingSink {
    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        let mut g = self.inner.lock().expect("RecordingSink lock poisoned");
        let now = tokio::time::Instant::now().duration_since(g.created_at);
        g.log.push((now, SinkCmd::Blank(mode)));
        g.blank_results.pop_front().unwrap_or(Ok(()))
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let mut g = self.inner.lock().expect("RecordingSink lock poisoned");
        let now = tokio::time::Instant::now().duration_since(g.created_at);
        g.log.push((now, SinkCmd::Wake));
        g.wake_results.pop_front().unwrap_or(Ok(()))
    }

    async fn wake_once(&self) -> Result<(), CmdFailure> {
        // RecordingSink has no retry semantics, so wake_once == wake.
        // Override anyway — the default forwards to wake, which is fine, but
        // a direct override makes the relationship explicit and lets tests
        // assert per-call counts against the wake_once path independently
        // should a future implementation diverge.
        self.wake().await
    }

    fn controller_health(&self) -> Vec<crate::rules::ControllerHealth> {
        self.inner
            .lock()
            .expect("RecordingSink lock poisoned")
            .health
            .clone()
    }
}

// ── RecordingRenderSink ────────────────────────────────────────────────────────

/// A single show/teardown call recorded by [`RecordingRenderSink`].
#[derive(Debug, Clone, PartialEq)]
pub enum RenderCmd {
    /// A show command (with generation, ladder index, and stage kind).
    Show {
        /// Stage generation counter.
        r#gen: u64,
        /// Index into the display's ladder.
        idx: usize,
        /// Stage kind — `RenderBlack` or `RenderScreensaver`.
        kind: StageKind,
    },
    /// A teardown command.
    Teardown {
        /// Stage generation counter.
        r#gen: u64,
    },
}

/// A [`RenderSink`] that records every call (with the virtual time at which
/// it was made) and serves scripted results for `show`.  `teardown` is
/// infallible by contract, so it is always recorded but never scripted.
///
/// Empty result queue means "default Ok".  Use
/// [`Self::push_show_result`] to enqueue scripted failures.
#[derive(Debug, Clone)]
pub struct RecordingRenderSink {
    /// Shared log + result queue — `Arc<Mutex<...>>` so the public API can
    /// hand out snapshots without disturbing the running engine.
    inner: Arc<Mutex<RenderInner>>,
}

#[derive(Debug)]
struct RenderInner {
    /// `(virtual_offset_from_creation, command)` for every issued call.
    log: Vec<(Duration, RenderCmd)>,
    /// Scripted show results — popped FIFO; empty means Ok.
    show_results: VecDeque<Result<(), CmdFailure>>,
    /// Monotonic instant captured at construction for virtual timestamps.
    created_at: tokio::time::Instant,
}

impl RecordingRenderSink {
    /// Create a fresh sink.  Use [`Clone`] to share between the engine and the
    /// test harness.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RenderInner {
                log: Vec::new(),
                show_results: VecDeque::new(),
                created_at: tokio::time::Instant::now(),
            })),
        }
    }

    /// Snapshot of every render command issued so far, oldest first.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (another task panicked while
    /// holding the lock).
    #[must_use]
    pub fn log(&self) -> Vec<(Duration, RenderCmd)> {
        self.inner
            .lock()
            .expect("RecordingRenderSink lock poisoned")
            .log
            .clone()
    }

    /// Push a scripted `show` result.  Drained FIFO by
    /// [`RenderSink::show`].
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (another task panicked while
    /// holding the lock).
    pub fn push_show_result(&self, result: Result<(), CmdFailure>) {
        self.inner
            .lock()
            .expect("RecordingRenderSink lock poisoned")
            .show_results
            .push_back(result);
    }
}

impl Default for RecordingRenderSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RenderSink for RecordingRenderSink {
    async fn show(&self, r#gen: u64, idx: usize, kind: StageKind) -> Result<(), CmdFailure> {
        let mut g = self
            .inner
            .lock()
            .expect("RecordingRenderSink lock poisoned");
        let now = tokio::time::Instant::now().duration_since(g.created_at);
        g.log.push((now, RenderCmd::Show { r#gen, idx, kind }));
        g.show_results.pop_front().unwrap_or(Ok(()))
    }

    async fn teardown(&self, r#gen: u64) {
        let mut g = self
            .inner
            .lock()
            .expect("RecordingRenderSink lock poisoned");
        let now = tokio::time::Instant::now().duration_since(g.created_at);
        g.log.push((now, RenderCmd::Teardown { r#gen }));
    }
}

// ── ExerciseSink — scriptable read_state for ControlMsg::Exercise tests ────────

/// A [`CommandSink`] test double for the control-path verification
/// feature. Records every blank/wake call and lets each test program a
/// script of [`PanelState`] snapshots that `read_state()` returns in
/// sequence.
///
/// Why not extend `RecordingSink`?  The exercise semantics are qualitatively
/// different (we care about state transitions across commands, not just
/// command counts), and keeping the readback script on its own surface
/// makes the test fakes easier to read — the `RecordingSink` log stays
/// command-only, the `ExerciseSink` log carries every panel observation.
///
/// Three semantics the tests pin:
/// 1. State-script exhaustion ⇒ `None` (we have no further observation,
///    so the next step reads "Unconfirmable" rather than fabricating a
///    state).
/// 2. Every blank/wake call is recorded regardless of the readback —
///    the test asserts command count independently of state changes.
/// 3. The wake-path-sacred guarantee is enforced by `wakes_issued`
///    (count of wake calls) which the fail-safe test asserts on.
#[derive(Debug, Clone)]
pub struct ExerciseSink {
    inner: Arc<Mutex<ExerciseInner>>,
}

#[derive(Debug)]
struct ExerciseInner {
    /// Every blank/wake call, oldest first (so a fail-safe test can
    /// assert the last entry was a wake).
    log: Vec<SinkCmd>,
    /// Scripted `read_state()` responses — popped FIFO. Empty ⇒ `None`.
    read_states: VecDeque<Option<PanelState>>,
    /// Scripted blank results — popped FIFO. Empty ⇒ `Ok(())`.
    blank_results: VecDeque<Result<(), CmdFailure>>,
    /// Scripted wake results — popped FIFO. Empty ⇒ `Ok(())`.
    wake_results: VecDeque<Result<(), CmdFailure>>,
    /// Monotonic instant captured at construction for log timestamps.
    #[allow(dead_code)]
    created_at: tokio::time::Instant,
    /// Monotonic instant captured when [`Self::wake`] was last called.
    /// Tests assert on this to confirm the final step in an error-mid-
    /// exercise scenario was a wake.
    last_wake_at: Option<tokio::time::Instant>,
}

impl ExerciseSink {
    /// Construct an empty [`ExerciseSink`] — no scripted reads or
    /// command results, every `read_state()` returns `None` and every
    /// blank/wake command returns `Ok(())` (the queue-empty default).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ExerciseInner {
                log: Vec::new(),
                read_states: VecDeque::new(),
                blank_results: VecDeque::new(),
                wake_results: VecDeque::new(),
                created_at: tokio::time::Instant::now(),
                last_wake_at: None,
            })),
        }
    }

    /// Append one scripted `read_state()` response. `None` means "the
    /// controller has no readback for this call" — the exercise handler
    /// surfaces that as `Unconfirmable`.
    pub fn push_read_state(&self, state: Option<PanelState>) {
        self.inner
            .lock()
            .expect("ExerciseSink lock poisoned")
            .read_states
            .push_back(state);
    }

    /// Append one scripted `blank()` result. Default when the queue is
    /// empty is `Ok(())`.
    pub fn push_blank_result(&self, result: Result<(), CmdFailure>) {
        self.inner
            .lock()
            .expect("ExerciseSink lock poisoned")
            .blank_results
            .push_back(result);
    }

    /// Append one scripted `wake()` result. Default when the queue is
    /// empty is `Ok(())`.
    pub fn push_wake_result(&self, result: Result<(), CmdFailure>) {
        self.inner
            .lock()
            .expect("ExerciseSink lock poisoned")
            .wake_results
            .push_back(result);
    }

    /// Snapshot of every blank/wake call, oldest first.
    #[must_use]
    pub fn log(&self) -> Vec<SinkCmd> {
        self.inner
            .lock()
            .expect("ExerciseSink lock poisoned")
            .log
            .clone()
    }

    /// Number of `wake()` calls recorded (success or failure — the count
    /// matters for the fail-safe test).
    #[must_use]
    pub fn wakes_issued(&self) -> usize {
        self.inner
            .lock()
            .expect("ExerciseSink lock poisoned")
            .log
            .iter()
            .filter(|c| matches!(c, SinkCmd::Wake))
            .count()
    }
}

impl Default for ExerciseSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CommandSink for ExerciseSink {
    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        let mut g = self.inner.lock().expect("ExerciseSink lock poisoned");
        g.log.push(SinkCmd::Blank(mode));
        g.blank_results.pop_front().unwrap_or(Ok(()))
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let mut g = self.inner.lock().expect("ExerciseSink lock poisoned");
        let now = tokio::time::Instant::now();
        g.log.push(SinkCmd::Wake);
        g.last_wake_at = Some(now);
        g.wake_results.pop_front().unwrap_or(Ok(()))
    }

    async fn wake_once(&self) -> Result<(), CmdFailure> {
        // wake_once == wake for the exercise semantics — the executor's
        // bounded-retry variant doesn't matter for the control-path test.
        self.wake().await
    }

    fn controller_health(&self) -> Vec<crate::rules::ControllerHealth> {
        Vec::new()
    }

    async fn read_state(&self) -> Option<PanelState> {
        let mut g = self.inner.lock().expect("ExerciseSink lock poisoned");
        g.read_states.pop_front().unwrap_or(None)
    }
}

// ── ProgrammableController — DisplayController read_state override for tests ────

/// A scripted [`DisplayController`] for tests that want to verify the
/// per-controller `read_state` override path (rather than the
/// `CommandSink` chain-walk the executor provides).  Not used by the
/// exercise end-to-end tests directly — those use [`ExerciseSink`], a
/// `CommandSink` — but documented here because it is the canonical
/// `DisplayController` read-state fake and shows up in the controller-
/// override tests.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ProgrammableController {
    name: &'static str,
    supported_modes: Vec<BlankMode>,
    read_state_script: Arc<Mutex<Vec<Option<PanelState>>>>,
    blank_result: Arc<Mutex<Result<(), CmdFailure>>>,
    wake_result: Arc<Mutex<Result<(), CmdFailure>>>,
}

#[allow(dead_code)]
impl ProgrammableController {
    /// Construct a controller with `Ok(())` defaults for every command
    /// and an empty `read_state` script (so `read_state()` returns
    /// `None`).
    #[must_use]
    pub fn new(name: &'static str, supported_modes: Vec<BlankMode>) -> Self {
        Self {
            name,
            supported_modes,
            read_state_script: Arc::new(Mutex::new(Vec::new())),
            blank_result: Arc::new(Mutex::new(Ok(()))),
            wake_result: Arc::new(Mutex::new(Ok(()))),
        }
    }

    /// Append one scripted `read_state()` response.  Drained FIFO.
    pub fn push_read_state(&self, state: Option<PanelState>) {
        self.read_state_script
            .lock()
            .expect("ProgrammableController lock poisoned")
            .push(state);
    }

    /// Override the result returned by [`Self::blank`].
    pub fn set_blank_result(&self, result: Result<(), CmdFailure>) {
        *self
            .blank_result
            .lock()
            .expect("ProgrammableController lock poisoned") = result;
    }

    /// Override the result returned by [`Self::wake`].
    pub fn set_wake_result(&self, result: Result<(), CmdFailure>) {
        *self
            .wake_result
            .lock()
            .expect("ProgrammableController lock poisoned") = result;
    }
}

#[async_trait]
#[allow(dead_code)]
impl DisplayController for ProgrammableController {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        self.supported_modes.clone()
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
        self.blank_result
            .lock()
            .expect("ProgrammableController lock poisoned")
            .clone()
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        self.wake_result
            .lock()
            .expect("ProgrammableController lock poisoned")
            .clone()
    }

    async fn read_state(&self) -> Option<PanelState> {
        let mut script = self
            .read_state_script
            .lock()
            .expect("ProgrammableController lock poisoned");
        if script.is_empty() {
            None
        } else {
            script.remove(0)
        }
    }
}
