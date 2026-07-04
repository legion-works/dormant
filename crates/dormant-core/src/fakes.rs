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

use crate::traits::{CommandSink, SensorSource};
use crate::types::{BlankMode, CmdFailure, PresenceEvent};

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
}
