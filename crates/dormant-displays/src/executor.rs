//! Per-display [`DisplayExecutor`] — the wake-reliability core of `dormant`.
//!
//! Each display has one [`DisplayExecutor`] wrapping an *ordered chain* of
//! [`DisplayController`]s (e.g. `kwin-dpms` first, `ddcci` as fallback). The
//! executor implements [`dormant_core::traits::CommandSink`], so the rules
//! engine sees one object per display and is unaware of fallback or retry
//! behavior.
//!
//! ## Why one object per display?
//!
//! - The rules engine's [`dormant_core::state_machine`] issues commands
//!   against a [`DisplayId`], not a controller; the executor hides the
//!   controller chain.
//! - Wake retries must be scoped to *one* display — a flaky `KWin` on one
//!   monitor must not delay wakes on a different monitor.
//! - The "supersede in-flight" semantics (below) only make sense within one
//!   display: a wake on display A must not be interrupted by a blank on
//!   display B.
//!
//! ## Blank semantics
//!
//! Iterate the chain in order. For each controller that is both available
//! and supports the requested mode, attempt `blank(mode)`. The first Ok
//! wins. Per-controller failures are logged at `warn` and the next controller
//! is tried; the all-fail case logs `error` and returns a
//! [`dormant_core::types::CmdFailure`] whose `controller` field is the name
//! of the *last* controller attempted (or `"none-eligible"` if nothing was
//! eligible).
//!
//! ## Wake semantics — two-layer retry
//!
//! This executor owns the **inner** retry layer: a bounded *burst* of
//! `(initial + wake_retries)` rounds, with each round iterating the full
//! chain. Between rounds the executor sleeps for
//! `wake_retry_backoff * 2^round_index`, cancellation-aware so a fresh
//! blank can supersede the wake mid-burst (see below).
//!
//! The **outer** retry layer lives in
//! [`dormant_core::state_machine::DisplayStateMachine`]: when a wake
//! exhausts the inner burst, the state machine schedules another
//! `IssueWake` at `wake_retry_interval`. The two layers compose — a stuck
//! display will be retried forever (with exponentially growing gaps), but a
//! single transient failure is recovered in milliseconds without bothering
//! the state machine.
//!
//! ## Supersede semantics
//!
//! A [`CancellationToken`] is held in a `Mutex<Option<…>>`. Entering either
//! `blank()` or `wake()` swaps in a fresh token and cancels the previous
//! one, so:
//!
//! - the *between-round* sleep of an in-flight wake can be interrupted the
//!   instant a blank arrives (`tokio::select!` on the token vs. sleep);
//! - a fresh blank arriving *during* a round can skip the next controller
//!   in the chain — `is_cancelled()` is checked before each `wake()` call
//!   and an Err is returned immediately. The controller calls themselves
//!   are not cancelled mid-flight (they're short, each has its own
//!   timeout), so they surface a clean `CmdFailure` that the state
//!   machine's `cmd_gen` stale-detection discards.
//!
//! An empty controller chain short-circuits `wake()` to an immediate
//! `"none-eligible"` Err — the inter-round backoff must never fire when
//! there is no chain to iterate.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::{DormantError, E_BLANK_FAILED, E_WAKE_FAILED};
use dormant_core::rules::{ControllerHealth, ControllerRole};
use dormant_core::traits::{CommandSink, DisplayController, PanelState};
use dormant_core::types::{BlankMode, CmdFailure, DisplayId};
use tokio_util::sync::CancellationToken;

// ── RetrySettings ──────────────────────────────────────────────────────────────

/// Bounded retry parameters for the executor's wake burst.
///
/// Sourced from [`dormant_core::config::schema::RuleConfig`] fields
/// `wake_retries` (rounds after the initial attempt) and
/// `wake_retry_backoff` (base backoff; doubles per round).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    /// Number of *retry* rounds after the initial attempt. Total rounds in a
    /// burst = `wake_retries + 1`.
    pub wake_retries: u32,
    /// Base backoff between rounds. Doubles per round index.
    pub wake_retry_backoff: Duration,
}

// ── DisplayExecutor ────────────────────────────────────────────────────────────

/// Per-display executor: ordered controller chain + supersede-aware retry.
///
/// Constructed once per display by [`crate::registry::build_controllers`] and
/// handed to [`dormant_core::rules::RulesEngine`] as a
/// [`CommandSink`].
pub struct DisplayExecutor {
    /// Display this executor drives (for logging).
    display: DisplayId,
    /// Ordered chain — first controller is preferred.
    chain: Vec<Box<dyn DisplayController>>,
    /// The mode the rules engine will request on the blank path. Stored for
    /// the Task-16 post-probe validator (`effective_modes` ⊇ `effective_mode`);
    /// the executor itself takes the mode per call from the [`CommandSink`].
    #[allow(dead_code)]
    effective_mode: BlankMode,
    /// Wake retry parameters.
    retry: RetrySettings,
    /// The current in-flight command's cancellation token. `None` between
    /// commands.
    supersede: Mutex<Option<CancellationToken>>,
    /// Per-controller health from the last blank/wake attempt.  `Arc<Mutex<…>>`
    /// so [`CommandSink::controller_health`] (sync, `&self`) can return a
    /// snapshot even when spawned tasks are writing this field.
    health: std::sync::Arc<std::sync::Mutex<Vec<ControllerHealth>>>,
    /// Chain index of the controller that performed the *last successful*
    /// `blank()`. `None` before any successful blank, or once a wake
    /// attempted by this owner has itself succeeded.
    ///
    /// In-memory only — never serialized (no IPC/config/serde surface).
    /// `wake()`/`wake_once()` consult this so a display that was blanked by
    /// controller B (because A was unavailable or A doesn't support the
    /// blank mode) is *woken* by B first too, even if A has since become
    /// available and would otherwise win the normal chain-order race.
    /// Ownership is stronger than probe availability: the owner is always
    /// attempted first, `is_available()` or not (see `wake`/`wake_once`).
    ///
    /// Concurrency note: in production, `blank()` can be entered concurrently
    /// on the same executor — the rules dispatch fires `blank`/`wake` off a
    /// fire-and-forget `tokio::spawn`, and the emergency-wake / doctor
    /// `--exercise` paths share the same `CommandSink` handle. Two in-flight
    /// `blank()` calls can therefore complete out of logical order, and this
    /// field is a plain last-write-wins `Mutex<Option<usize>>` with no
    /// generation/ordering check — the same accepted model already used for
    /// the `health` field above. A stale write here just means `blank_owner`
    /// can momentarily point at a controller that isn't the *logically*
    /// latest blank's controller. That is not a correctness failure: the
    /// worst case is one wasted owner-first attempt on the wrong controller
    /// before the wake falls through the rest of the chain in normal order,
    /// exactly as if there had been no owner at all.
    blank_owner: std::sync::Mutex<Option<usize>>,
}

impl DisplayExecutor {
    /// Construct an executor.
    #[must_use]
    pub fn new(
        display: DisplayId,
        controllers: Vec<Box<dyn DisplayController>>,
        effective_mode: BlankMode,
        retry: RetrySettings,
    ) -> Self {
        Self {
            display,
            chain: controllers,
            effective_mode,
            retry,
            supersede: Mutex::new(None),
            health: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            blank_owner: std::sync::Mutex::new(None),
        }
    }

    /// Run each controller's one-time [`DisplayController::probe`] in chain
    /// order. Failures are logged and the controller stays in the chain —
    /// `is_available` / `supported_modes` decide actual use at command time.
    ///
    /// The returned vector preserves chain order so callers can correlate
    /// outcomes with the configured chain.
    pub async fn probe_all(&mut self) -> Vec<(String, Result<(), DormantError>)> {
        let mut out = Vec::with_capacity(self.chain.len());
        for controller in &mut self.chain {
            let name = controller.name().to_string();
            let result = controller.probe().await;
            if let Err(ref e) = result {
                tracing::warn!(
                    event = "display_probe_failed",
                    display = %self.display,
                    controller = %name,
                    error = %e,
                    "controller probe failed; staying in chain",
                );
            }
            out.push((name, result));
        }
        out
    }

    /// Union of every controller's `supported_modes()`.
    ///
    /// Used to validate that a display config's `blank_mode` and
    /// degraded-mode fallback are members of this set.
    #[must_use]
    pub fn effective_modes(&self) -> Vec<BlankMode> {
        let mut seen: HashSet<BlankMode> = HashSet::new();
        let mut out: Vec<BlankMode> = Vec::new();
        for c in &self.chain {
            for m in c.supported_modes() {
                if seen.insert(m) {
                    out.push(m);
                }
            }
        }
        out
    }

    /// Swap in a fresh supersede token and cancel the previous one. Returns
    /// the new token (held by the caller for cancellation-aware sleeps).
    fn rotate_supersede(&self) -> CancellationToken {
        let new_token = CancellationToken::new();
        let old_token = {
            let mut guard = self
                .supersede
                .lock()
                .expect("DisplayExecutor supersede lock poisoned");
            let old = guard.take();
            *guard = Some(new_token.clone());
            old
        };
        if let Some(old) = old_token {
            old.cancel();
        }
        new_token
    }

    /// Build the attempt order for a wake pass: the recorded blank `owner`
    /// (if any and still in range) first, then every other chain index in
    /// original configured order. Used by both `wake_once` and every round
    /// of `wake`'s retry burst so the two paths cannot drift apart.
    ///
    /// A stale owner index (chain shrank — not possible today, but kept
    /// defensive) is simply dropped rather than panicking.
    fn owner_first_order(owner: Option<usize>, len: usize) -> Vec<usize> {
        let owner = owner.filter(|&o| o < len);
        let mut order = Vec::with_capacity(len);
        if let Some(o) = owner {
            order.push(o);
        }
        for i in 0..len {
            if Some(i) != owner {
                order.push(i);
            }
        }
        order
    }

    /// Snapshot of the current blank owner — test-only introspection.
    #[cfg(test)]
    fn blank_owner_for_test(&self) -> Option<usize> {
        *self
            .blank_owner
            .lock()
            .expect("DisplayExecutor blank_owner lock poisoned")
    }
}

#[async_trait]
impl CommandSink for DisplayExecutor {
    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        let _supersede_token = self.rotate_supersede();

        let mut last_controller = String::from("none-eligible");
        let mut eligible_count: usize = 0;
        // One slot per chain position — updated in place so skipped
        // controllers are never masked (Must 2b).
        let mut health: Vec<ControllerHealth> = self
            .chain
            .iter()
            .enumerate()
            .map(|(i, c)| ControllerHealth {
                name: c.name().to_string(),
                role: if i == 0 {
                    ControllerRole::Primary
                } else {
                    ControllerRole::Fallback
                },
                healthy: false,
                detail: None,
            })
            .collect();

        for (i, controller) in self.chain.iter().enumerate() {
            if !controller.is_available().await {
                health[i].healthy = false;
                health[i].detail = Some("controller unavailable".to_string());
                continue;
            }
            if !controller.supported_modes().contains(&mode) {
                health[i].healthy = false;
                health[i].detail = Some("mode not supported".to_string());
                continue;
            }
            eligible_count += 1;
            match controller.blank(mode).await {
                Ok(()) => {
                    health[i].healthy = true;
                    health[i].detail = None;
                    *self
                        .health
                        .lock()
                        .expect("DisplayExecutor health lock poisoned") = health;
                    // Record the owner only now, after the result that
                    // licenses it (this Ok) is final — a later successful
                    // blank replaces the owner; an all-failed blank (below)
                    // never reaches this line, so the prior owner survives.
                    *self
                        .blank_owner
                        .lock()
                        .expect("DisplayExecutor blank_owner lock poisoned") = Some(i);
                    return Ok(());
                }
                Err(e) => {
                    health[i].healthy = false;
                    health[i].detail = Some(e.to_string());
                    tracing::warn!(
                        event = "blank_controller_failed",
                        display = %self.display,
                        controller = controller.name(),
                        error = %e,
                    );
                    last_controller = controller.name().to_string();
                }
            }
        }

        tracing::error!(
            event = "blank_failed",
            display = %self.display,
            mode = ?mode,
            eligible = eligible_count,
            last_controller = %last_controller,
            "blank failed across the entire chain",
        );
        *self
            .health
            .lock()
            .expect("DisplayExecutor health lock poisoned") = health;
        Err(CmdFailure {
            controller: last_controller,
            error: format!("{E_BLANK_FAILED}: no controller succeeded (mode={mode:?})"),
        })
    }

    async fn wake_once(&self) -> Result<(), CmdFailure> {
        // Single round through the chain — no retries, no backoff. Used by
        // the emergency-wake path so a panic-recovery command returns fast.
        let _supersede_token = self.rotate_supersede();

        if self.chain.is_empty() {
            return Err(CmdFailure {
                controller: "none-eligible".into(),
                error: format!("{E_WAKE_FAILED}: empty controller chain"),
            });
        }

        let owner = *self
            .blank_owner
            .lock()
            .expect("DisplayExecutor blank_owner lock poisoned");
        let order = Self::owner_first_order(owner, self.chain.len());

        for i in order {
            let controller = &self.chain[i];
            let is_owner_attempt = owner == Some(i);
            // Ownership is stronger than probe availability: the owner is
            // attempted regardless of `is_available()`. Non-owner
            // candidates are still gated as before.
            if !is_owner_attempt && !controller.is_available().await {
                continue;
            }
            if controller.wake().await.is_ok() {
                if is_owner_attempt {
                    *self
                        .blank_owner
                        .lock()
                        .expect("DisplayExecutor blank_owner lock poisoned") = None;
                }
                return Ok(());
            }
        }

        Err(CmdFailure {
            controller: "exhausted".into(),
            error: format!("{E_WAKE_FAILED}: no controller succeeded in single attempt"),
        })
    }

    #[allow(clippy::too_many_lines)] // owner-first ordering + per-round retry/backoff/supersede bookkeeping form one sequential protocol; extracting helpers would scatter the invariants
    async fn wake(&self) -> Result<(), CmdFailure> {
        let supersede_token = self.rotate_supersede();

        // Empty-chain short-circuit: never enter the retry loop (and its
        // inter-round sleeps) when there is nothing to iterate. A display
        // misconfigured with no controllers should fail immediately, not
        // burn through N×backoff virtual time first.
        if self.chain.is_empty() {
            return Err(CmdFailure {
                controller: "none-eligible".to_string(),
                error: format!("{E_WAKE_FAILED}: empty controller chain"),
            });
        }

        let total_rounds = self
            .retry
            .wake_retries
            .checked_add(1)
            .expect("wake_retries overflow");

        // One slot per chain position — updated in place across retries so
        // the final Vec has exactly one row per controller (Must 2a).
        let mut health: Vec<ControllerHealth> = self
            .chain
            .iter()
            .enumerate()
            .map(|(i, c)| ControllerHealth {
                name: c.name().to_string(),
                role: if i == 0 {
                    ControllerRole::Primary
                } else {
                    ControllerRole::Fallback
                },
                healthy: false,
                detail: None,
            })
            .collect();

        // Read the blank owner once, before the burst starts: any blank
        // arriving mid-burst supersedes this wake via `rotate_supersede()`
        // (checked below), so the owner cannot change out from under a
        // wake that is still running. The same order is reused for every
        // round — one helper, so `wake_once` and `wake` cannot drift.
        let owner = *self
            .blank_owner
            .lock()
            .expect("DisplayExecutor blank_owner lock poisoned");
        let order = Self::owner_first_order(owner, self.chain.len());

        for round in 0..total_rounds {
            for &i in &order {
                let controller = &self.chain[i];
                let is_owner_attempt = owner == Some(i);
                // Mid-round supersede: a blank arriving between controller
                // calls aborts the rest of the chain (and the burst) without
                // waiting for the next inter-round sleep. The token was
                // swapped-and-cancelled by the blank's `rotate_supersede()`.
                if supersede_token.is_cancelled() {
                    // Do not update health on supersede — no real controller
                    // attempt was made here.
                    return Err(CmdFailure {
                        controller: "superseded".to_string(),
                        error: format!("{E_WAKE_FAILED}: superseded by blank"),
                    });
                }
                // Ownership is stronger than probe availability: the owner
                // is attempted regardless of `is_available()`. Non-owner
                // candidates are still gated as before.
                if !is_owner_attempt && !controller.is_available().await {
                    health[i].healthy = false;
                    health[i].detail = Some("controller unavailable".to_string());
                    continue;
                }
                // Wake is mode-independent: any available controller is
                // eligible to wake the display, regardless of which blank
                // modes it supports.
                match controller.wake().await {
                    Ok(()) => {
                        // Must 1: a blank that superseded while this wake
                        // call was in flight must NOT commit wake health and
                        // must NOT return Ok — the blank is the last command.
                        if supersede_token.is_cancelled() {
                            return Err(CmdFailure {
                                controller: "superseded".to_string(),
                                error: format!("{E_WAKE_FAILED}: superseded by blank"),
                            });
                        }
                        health[i].healthy = true;
                        health[i].detail = None;
                        *self
                            .health
                            .lock()
                            .expect("DisplayExecutor health lock poisoned") = health;
                        if is_owner_attempt {
                            // The owner's wake succeeded: clear it. A
                            // non-owner fallback success (below, implicitly
                            // — `is_owner_attempt` is false there) leaves
                            // the owner in place, per invariant.
                            *self
                                .blank_owner
                                .lock()
                                .expect("DisplayExecutor blank_owner lock poisoned") = None;
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        health[i].healthy = false;
                        health[i].detail = Some(e.to_string());
                        tracing::warn!(
                            event = "wake_controller_failed",
                            display = %self.display,
                            controller = controller.name(),
                            round,
                            error = %e,
                        );
                    }
                }
            }

            // Between-round backoff: double per round index. Cancellable so a
            // fresh blank can interrupt a stuck wake burst.
            if round + 1 < total_rounds {
                let multiplier = 1u32.checked_shl(round).unwrap_or(u32::MAX);
                let backoff = self.retry.wake_retry_backoff.saturating_mul(multiplier);
                tokio::select! {
                    () = supersede_token.cancelled() => {
                        return Err(CmdFailure {
                            controller: "superseded".to_string(),
                            error: format!("{E_WAKE_FAILED}: superseded by blank"),
                        });
                    }
                    () = tokio::time::sleep(backoff) => {}
                }
            }
        }

        tracing::error!(
            event = "wake_failed",
            display = %self.display,
            rounds = total_rounds,
            "wake burst exhausted",
        );
        *self
            .health
            .lock()
            .expect("DisplayExecutor health lock poisoned") = health;
        Err(CmdFailure {
            controller: "exhausted".to_string(),
            error: format!("{E_WAKE_FAILED}: burst exhausted after {total_rounds} rounds"),
        })
    }

    fn controller_health(&self) -> Vec<ControllerHealth> {
        self.health
            .lock()
            .expect("DisplayExecutor health lock poisoned")
            .clone()
    }

    /// Walk the configured chain and return the first non-`None` panel
    /// state reported by a controller's [`DisplayController::read_state`].
    ///
    /// Mirrors the chain semantics used by `blank` / `wake`: the first
    /// controller that can read the panel wins.  Used by the
    /// `ControlMsg::Exercise` handler so a chain with a primary controller
    /// that has no readback falls through to a fallback that does
    /// (e.g. a `samsung-tizen` primary that has no port-1516 backlight
    /// paired with a `ddcci` fallback that does).  Returns `None` if no
    /// controller in the chain supports readback — the honest answer that
    /// the exercise handler renders as `Unconfirmable`.
    async fn read_state(&self) -> Option<PanelState> {
        for controller in &self.chain {
            if let Some(state) = controller.read_state().await {
                return Some(state);
            }
        }
        None
    }

    /// Walk the configured chain and return the first non-`None` panel
    /// state reported by a controller's
    /// [`DisplayController::read_state_sampled`] — the sampler-priority
    /// twin of [`Self::read_state`], used by the periodic wear-tracking
    /// poll (spec §4.3).
    ///
    /// This override is **mandatory**, not cosmetic (P1): the
    /// [`CommandSink::read_state_sampled`] trait default merely delegates
    /// to `read_state()` (command priority), which would silently drop
    /// sampler priority at exactly this boundary — the one place the
    /// engine ever talks to a display is through this trait object, not
    /// through the concrete controller chain.
    async fn read_state_sampled(&self) -> Option<PanelState> {
        for controller in &self.chain {
            if let Some(state) = controller.read_state_sampled().await {
                return Some(state);
            }
        }
        None
    }

    /// Walk the configured chain and return the first non-`None`
    /// usage-hours reading — mirrors [`Self::read_state`]'s chain-walk
    /// shape exactly. Used once, at wear-ledger seeding time (task T7),
    /// so a chain with a primary controller that has no usage-hours
    /// readback (e.g. `command`, `kwin-dpms`) falls through to a `ddcci`
    /// fallback that does.
    async fn read_usage_hours(&self) -> Option<u32> {
        for controller in &self.chain {
            if let Some(hours) = controller.read_usage_hours().await {
                return Some(hours);
            }
        }
        None
    }

    /// Walk the configured chain and return the first non-`None`
    /// panel-derived identity — mirrors [`Self::read_usage_hours`]'s
    /// chain-walk shape exactly (T7 review M1). Used once, at wear-ledger
    /// creation time, so a chain with a primary controller that has no
    /// panel identity (e.g. `command`, `kwin-dpms`) falls through to a
    /// `ddcci`/`samsung-tizen` fallback that does.
    fn panel_identity(&self) -> Option<String> {
        for controller in &self.chain {
            if let Some(id) = controller.panel_identity() {
                return Some(id);
            }
        }
        None
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    // ── FakeController ─────────────────────────────────────────────────────

    /// Scripted [`DisplayController`] for executor tests — shared via
    /// `Arc<Mutex<…>>` so the test can inspect the call log after the
    /// executor has returned.
    #[derive(Clone)]
    struct FakeController {
        name: &'static str,
        inner: Arc<Mutex<FakeInner>>,
    }

    #[derive(Default)]
    struct FakeInner {
        modes: Vec<BlankMode>,
        available: bool,
        blank_results: VecDeque<Result<(), CmdFailure>>,
        wake_results: VecDeque<Result<(), CmdFailure>>,
        log: Vec<(String, &'static str)>,
        /// Optional delay applied at the START of `blank()` (after logging)
        /// — used by mid-round supersede tests so a controller "occupies"
        /// the burst for a measurable amount of virtual time.
        blank_delay: Duration,
        /// Same as `blank_delay` but for `wake()`.
        wake_delay: Duration,
        /// Scripted [`DisplayController::read_usage_hours`] response — used
        /// by the T5 chain-walk test (test 2).
        usage_hours: Option<u32>,
        /// Scripted [`DisplayController::panel_identity`] response — used
        /// by the T7 chain-walk test.
        panel_identity: Option<String>,
    }

    impl FakeController {
        fn new(name: &'static str, modes: Vec<BlankMode>) -> Self {
            Self {
                name,
                inner: Arc::new(Mutex::new(FakeInner {
                    modes,
                    available: true,
                    ..Default::default()
                })),
            }
        }

        fn set_available(&self, v: bool) {
            self.inner.lock().unwrap().available = v;
        }

        fn push_blank_result(&self, r: Result<(), CmdFailure>) {
            self.inner.lock().unwrap().blank_results.push_back(r);
        }

        fn push_wake_result(&self, r: Result<(), CmdFailure>) {
            self.inner.lock().unwrap().wake_results.push_back(r);
        }

        #[allow(dead_code)] // not all tests exercise the blank path with a delay
        fn set_blank_delay(&self, d: Duration) {
            self.inner.lock().unwrap().blank_delay = d;
        }

        fn set_wake_delay(&self, d: Duration) {
            self.inner.lock().unwrap().wake_delay = d;
        }

        fn set_usage_hours(&self, hours: Option<u32>) {
            self.inner.lock().unwrap().usage_hours = hours;
        }

        fn set_panel_identity(&self, id: Option<String>) {
            self.inner.lock().unwrap().panel_identity = id;
        }

        fn count_op(&self, op: &'static str) -> usize {
            self.inner
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|(_, o)| *o == op)
                .count()
        }
    }

    #[async_trait]
    impl DisplayController for FakeController {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supported_modes(&self) -> Vec<BlankMode> {
            self.inner.lock().unwrap().modes.clone()
        }

        async fn is_available(&self) -> bool {
            let mut g = self.inner.lock().unwrap();
            g.log.push((self.name.to_string(), "is_available"));
            g.available
        }

        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            // Log first, then read+clear the delay under the lock so we
            // don't hold the lock across an await.
            let delay = {
                let mut g = self.inner.lock().unwrap();
                g.log.push((self.name.to_string(), "blank"));
                g.blank_delay
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let mut g = self.inner.lock().unwrap();
            g.blank_results.pop_front().unwrap_or(Ok(()))
        }

        async fn wake(&self) -> Result<(), CmdFailure> {
            let delay = {
                let mut g = self.inner.lock().unwrap();
                g.log.push((self.name.to_string(), "wake"));
                g.wake_delay
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let mut g = self.inner.lock().unwrap();
            g.wake_results.pop_front().unwrap_or(Ok(()))
        }

        async fn read_usage_hours(&self) -> Option<u32> {
            self.inner.lock().unwrap().usage_hours
        }

        fn panel_identity(&self) -> Option<String> {
            self.inner.lock().unwrap().panel_identity.clone()
        }
    }

    fn cmd_failure(name: &str, msg: &str) -> CmdFailure {
        CmdFailure {
            controller: name.to_string(),
            error: msg.to_string(),
        }
    }

    fn err(name: &str) -> CmdFailure {
        cmd_failure(name, &format!("{E_WAKE_FAILED}: scripted failure"))
    }

    fn executor_with(
        controllers: Vec<FakeController>,
        retry: RetrySettings,
    ) -> (Arc<DisplayExecutor>, Vec<FakeController>) {
        let boxed: Vec<Box<dyn DisplayController>> = controllers
            .iter()
            .cloned()
            .map(|c| Box::new(c) as Box<dyn DisplayController>)
            .collect();
        let exec = Arc::new(DisplayExecutor::new(
            DisplayId("test-display".into()),
            boxed,
            BlankMode::PowerOff,
            retry,
        ));
        (exec, controllers)
    }

    fn default_retry() -> RetrySettings {
        RetrySettings {
            wake_retries: 0,
            wake_retry_backoff: Duration::from_secs(1),
        }
    }

    // ── blank tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn blank_first_eligible_controller_wins() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.push_blank_result(Ok(()));
        let (exec, handles) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();

        assert_eq!(a.count_op("blank"), 1, "A called once");
        assert_eq!(b.count_op("blank"), 0, "B not called");
        assert_eq!(handles[0].count_op("blank"), 1);
        assert_eq!(handles[1].count_op("blank"), 0);
    }

    #[tokio::test]
    async fn blank_falls_through_on_error() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.push_blank_result(Err(err("A")));
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();

        assert_eq!(a.count_op("blank"), 1, "A tried");
        assert_eq!(b.count_op("blank"), 1, "B tried after A failed");
    }

    #[tokio::test]
    async fn blank_skips_unavailable_and_mode_mismatch() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.set_available(false);
        // B supports PowerOff but we'll ask for BrightnessZero instead so
        // supported_modes filter kicks in.
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        let res = exec.blank(BlankMode::BrightnessZero).await.unwrap_err();
        assert_eq!(res.controller, "none-eligible");
        assert!(res.error.starts_with(E_BLANK_FAILED));
        assert_eq!(a.count_op("blank"), 0);
        assert_eq!(b.count_op("blank"), 0);
    }

    #[tokio::test]
    async fn blank_all_fail_returns_cmdfailure_with_last_controller() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.push_blank_result(Err(err("A")));
        b.push_blank_result(Err(err("B")));
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        let res = exec.blank(BlankMode::PowerOff).await.unwrap_err();
        assert_eq!(res.controller, "B", "last attempted controller");
        assert!(res.error.starts_with(E_BLANK_FAILED));
        assert_eq!(a.count_op("blank"), 1);
        assert_eq!(b.count_op("blank"), 1);
    }

    // ── wake tests ────────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn wake_retries_full_chain_per_round_with_backoff() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        // Round 0: both fail. Round 1: B succeeds.
        a.push_wake_result(Err(err("A")));
        b.push_wake_result(Err(err("B")));
        a.push_wake_result(Err(err("A")));
        b.push_wake_result(Ok(()));

        let retry = RetrySettings {
            wake_retries: 1,
            wake_retry_backoff: Duration::from_secs(7),
        };
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], retry);

        let start = tokio::time::Instant::now();
        exec.wake().await.unwrap();
        let elapsed = start.elapsed();

        // Round 0 (initial): A err, B err (2 calls).
        // Sleep backoff (round 0 multiplier = 1) → 7s.
        // Round 1 (retry): A err, B ok → return Ok (2 more calls).
        // Total: 4 wake calls. Elapsed == 1×backoff.
        assert_eq!(a.count_op("wake"), 2, "A tried in both rounds");
        assert_eq!(b.count_op("wake"), 2, "B tried in both rounds");
        assert_eq!(
            elapsed, retry.wake_retry_backoff,
            "elapsed should equal exactly one backoff sleep",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wake_exhausted_returns_err_after_initial_plus_n_rounds() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        // Both controllers always fail (script enough for 3 rounds × 2 = 6).
        for _ in 0..6 {
            a.push_wake_result(Err(err("A")));
            b.push_wake_result(Err(err("B")));
        }

        let retry = RetrySettings {
            wake_retries: 2,
            wake_retry_backoff: Duration::from_millis(100),
        };
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], retry);

        let res = exec.wake().await.unwrap_err();
        assert_eq!(res.controller, "exhausted");
        assert!(res.error.starts_with(E_WAKE_FAILED));

        // 3 rounds × 2 controllers = 6 wake calls.
        assert_eq!(a.count_op("wake"), 3);
        assert_eq!(b.count_op("wake"), 3);
    }

    #[tokio::test]
    async fn wake_mode_independent() {
        // B's supported_modes has zero overlap with PowerOff, but wake is
        // mode-independent and must still try B.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::ScreenOffAudioOn]);
        a.push_wake_result(Err(err("A")));
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.wake().await.unwrap();

        assert_eq!(a.count_op("wake"), 1, "A tried");
        assert_eq!(b.count_op("wake"), 1, "B tried despite zero mode overlap");
    }

    #[tokio::test(start_paused = true)]
    async fn blank_supersedes_inflight_wake() {
        // Both controllers always fail on wake — wake burst would otherwise
        // sleep the backoff between rounds.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        for _ in 0..10 {
            a.push_wake_result(Err(err("A")));
            b.push_wake_result(Err(err("B")));
        }
        // Blank should succeed on A.
        a.push_blank_result(Ok(()));

        let retry = RetrySettings {
            wake_retries: 5,
            wake_retry_backoff: Duration::from_secs(60),
        };
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], retry);

        let exec_for_wake = Arc::clone(&exec);
        let wake_task = tokio::spawn(async move { exec_for_wake.wake().await });

        // Let wake task reach its between-round sleep before blank arrives.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        // Issue blank — should cancel wake's token and proceed.
        let start = tokio::time::Instant::now();
        exec.blank(BlankMode::PowerOff).await.unwrap();
        let blank_elapsed = start.elapsed();
        assert!(
            blank_elapsed < Duration::from_secs(1),
            "blank should not be parked on the wake's long backoff; took {blank_elapsed:?}",
        );

        let wake_result = wake_task.await.unwrap();
        let err = wake_result.unwrap_err();
        assert_eq!(err.controller, "superseded");
        assert!(err.error.contains("superseded by blank"));
    }

    // ── misc ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn effective_modes_unions_chain() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff, BlankMode::ScreenOffAudioOn]);
        let b = FakeController::new("B", vec![BlankMode::BrightnessZero]);
        let c = FakeController::new(
            "C",
            vec![BlankMode::PowerOff], // duplicate, should be deduped
        );
        let (exec, _) = executor_with(vec![a, b, c], default_retry());

        let modes = exec.effective_modes();
        assert!(modes.contains(&BlankMode::PowerOff));
        assert!(modes.contains(&BlankMode::ScreenOffAudioOn));
        assert!(modes.contains(&BlankMode::BrightnessZero));
        assert_eq!(
            modes.len(),
            3,
            "PowerOff should appear exactly once (deduped across A and C)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn probe_all_returns_per_controller_results_in_chain_order() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        // We exercise probe_all by re-constructing a mutable executor.
        let boxed: Vec<Box<dyn DisplayController>> = vec![Box::new(a.clone()), Box::new(b.clone())];
        let mut exec = DisplayExecutor::new(
            DisplayId("probe-target".into()),
            boxed,
            BlankMode::PowerOff,
            default_retry(),
        );
        let results = exec.probe_all().await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "A");
        assert_eq!(results[1].0, "B");
        assert!(results.iter().all(|(_, r)| r.is_ok()));
    }

    // ── Should 2 — mid-round supersede ────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn supersede_cancels_wake_mid_round_before_second_controller() {
        // A's wake takes 100ms virtual then errors; B's wake would succeed
        // if reached. Without the mid-round supersede check, A would run,
        // then B would run (and return Ok) — but the spec says a blank that
        // arrives during the round must short-circuit the burst.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.set_wake_delay(Duration::from_millis(100));
        a.push_wake_result(Err(err("A")));
        // B would succeed if reached — no script needed (default Ok).

        let retry = RetrySettings {
            wake_retries: 0,
            wake_retry_backoff: Duration::from_secs(60),
        };
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], retry);

        let exec_for_wake = Arc::clone(&exec);
        let wake_task = tokio::spawn(async move { exec_for_wake.wake().await });

        // Yield repeatedly to let the wake task reach A.wake's sleep. The
        // current-thread runtime parks wake on its 100ms virtual sleep;
        // each yield_now gives wake another chance to progress until it
        // parks again.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        // Blank arrives mid-round — must cancel wake's token. A is
        // currently parked; B has not yet been called.
        exec.blank(BlankMode::PowerOff).await.unwrap();

        let result = wake_task.await.unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.controller, "superseded");
        assert!(err.error.contains("superseded by blank"));

        assert_eq!(a.count_op("wake"), 1, "A's wake ran once (was in-flight)");
        assert_eq!(
            b.count_op("wake"),
            0,
            "B was skipped by mid-round supersede"
        );
    }

    // ── Should 3 — backoff doubles per round ──────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn wake_backoff_doubles_per_round() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        let c = FakeController::new("C", vec![BlankMode::PowerOff]);
        // 4 rounds × 3 controllers = 12 calls. All fail.
        for _ in 0..12 {
            a.push_wake_result(Err(err("A")));
            b.push_wake_result(Err(err("B")));
            c.push_wake_result(Err(err("C")));
        }

        let base = Duration::from_millis(50);
        let retry = RetrySettings {
            wake_retries: 3,
            wake_retry_backoff: base,
        };
        let (exec, _) = executor_with(vec![a.clone(), b.clone(), c.clone()], retry);

        let start = tokio::time::Instant::now();
        let res = exec.wake().await;
        let elapsed = start.elapsed();

        let err = res.unwrap_err();
        assert_eq!(err.controller, "exhausted");

        // 4 rounds × 3 controllers = 12 wake calls.
        assert_eq!(a.count_op("wake"), 4);
        assert_eq!(b.count_op("wake"), 4);
        assert_eq!(c.count_op("wake"), 4);

        // Backoff doublings between rounds 0→1, 1→2, 2→3: 1+2+4 = 7×base.
        // This assertion distinguishes 2^round from (round+1) (the latter
        // would yield 1+2+3 = 6×base) and from a constant (4×base).
        assert_eq!(
            elapsed,
            base * 7,
            "backoff must double per round: 1+2+4 = 7×base",
        );
    }

    // ── Should 4 — empty-chain wake short-circuits ─────────────────────────

    #[tokio::test(start_paused = true)]
    async fn wake_empty_chain_errs_without_sleeping() {
        // Empty chain + a huge wake_retry_backoff: if the executor entered
        // its retry loop and slept even once, this test would burn real
        // time or virtual time. The short-circuit guarantees zero.
        let (exec, _) = executor_with(
            vec![],
            RetrySettings {
                wake_retries: 3,
                wake_retry_backoff: Duration::from_secs(3600),
            },
        );

        let start = tokio::time::Instant::now();
        let res = exec.wake().await;
        let elapsed = start.elapsed();

        let err = res.unwrap_err();
        assert_eq!(err.controller, "none-eligible");
        assert!(err.error.starts_with(E_WAKE_FAILED));
        assert_eq!(
            elapsed,
            Duration::ZERO,
            "empty-chain wake must not enter the retry loop",
        );
    }

    // ── controller_health ──────────────────────────────────────────────────

    #[tokio::test]
    async fn health_records_each_controller_primary_fail_fallback_ok() {
        let primary = FakeController::new("ddcci", vec![BlankMode::PowerOff]);
        let fallback = FakeController::new("kwin-dpms", vec![BlankMode::PowerOff]);
        // Primary fails, fallback succeeds.
        primary.push_blank_result(Err(err("ddcci")));
        let (exec, _) = executor_with(vec![primary.clone(), fallback.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();

        let health: Vec<ControllerHealth> = exec.controller_health();
        assert_eq!(health.len(), 2, "both controllers recorded");
        assert_eq!(health[0].name, "ddcci");
        assert_eq!(health[0].role, ControllerRole::Primary);
        assert!(!health[0].healthy, "primary failed");
        assert!(health[0].detail.is_some(), "failure detail recorded");
        assert_eq!(health[1].name, "kwin-dpms");
        assert_eq!(health[1].role, ControllerRole::Fallback);
        assert!(health[1].healthy, "fallback succeeded");
    }

    // ── Must-2a: one health slot per controller across wake retries ─────────

    #[tokio::test(start_paused = true)]
    async fn health_no_duplicate_rows_on_wake_retry() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        // Both always fail — enough for 3 rounds × 2 controllers = 6 calls.
        for _ in 0..6 {
            a.push_wake_result(Err(err("A")));
            b.push_wake_result(Err(err("B")));
        }
        let retry = RetrySettings {
            wake_retries: 2,
            wake_retry_backoff: Duration::from_millis(10),
        };
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], retry);

        let res = exec.wake().await;
        assert!(res.is_err(), "wake burst exhausts");

        let health = exec.controller_health();
        assert_eq!(
            health.len(),
            2,
            "exactly 2 slots — no duplicates per retry round"
        );
        assert_eq!(health[0].name, "A");
        assert_eq!(health[1].name, "B");
        assert!(!health[0].healthy);
        assert!(!health[1].healthy);
    }

    // ── Must-2b: unavailable primary + successful fallback shows both ───────

    #[tokio::test]
    async fn health_shows_unavailable_primary_and_successful_fallback() {
        let primary = FakeController::new("ddcci", vec![BlankMode::PowerOff]);
        let fallback = FakeController::new("kwin-dpms", vec![BlankMode::PowerOff]);
        primary.set_available(false);
        let (exec, _) = executor_with(vec![primary.clone(), fallback.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();

        let health = exec.controller_health();
        assert_eq!(health.len(), 2, "both controllers represented");
        assert_eq!(health[0].name, "ddcci");
        assert_eq!(health[0].role, ControllerRole::Primary);
        assert!(!health[0].healthy, "primary unavailable");
        assert!(
            health[0]
                .detail
                .as_ref()
                .is_some_and(|d| d.contains("unavailable")),
            "detail explains unavailability"
        );
        assert_eq!(health[1].name, "kwin-dpms");
        assert_eq!(health[1].role, ControllerRole::Fallback);
        assert!(health[1].healthy, "fallback succeeded");
    }

    // ── Must 1: superseded wake success does NOT overwrite blank health ─────

    #[tokio::test(start_paused = true)]
    async fn superseded_wake_success_preserves_blank_health() {
        // A delayed wake that would succeed, but gets superseded by a blank.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        // Wake will delay 100ms then succeed.
        a.set_wake_delay(Duration::from_millis(100));
        // Blank will succeed immediately on A.
        a.push_blank_result(Ok(()));
        let retry = RetrySettings {
            wake_retries: 0,
            wake_retry_backoff: Duration::from_secs(1),
        };
        let (exec, _) = executor_with(vec![a.clone()], retry);

        // Start a delayed wake.
        let exec_wake = Arc::clone(&exec);
        let wake_task = tokio::spawn(async move { exec_wake.wake().await });

        // Yield repeatedly until wake task reaches its 100ms sleep.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        // Issue blank — cancels wake's token, then blank succeeds.
        exec.blank(BlankMode::PowerOff).await.unwrap();

        // Wake task result: must be the superseded failure (Must 1).
        let wake_result = wake_task.await.unwrap();
        let err = wake_result.unwrap_err();
        assert_eq!(err.controller, "superseded");
        assert!(err.error.contains("superseded by blank"));

        // Health must reflect the BLANK result, not the wake (Must 1).
        let health = exec.controller_health();
        assert_eq!(health.len(), 1);
        assert!(health[0].healthy, "blank succeeded on A");
        assert_eq!(health[0].detail, None);
    }

    // ── T5: read_usage_hours chain-walk (test 2) ─────────────────────────────

    #[tokio::test]
    async fn read_usage_hours_chain_walk_primary_none_fallback_some() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        // A has no usage-hours readback (default None, mirroring a
        // `kwin-dpms`/`command` primary); B does (mirroring a `ddcci`
        // fallback).
        b.set_usage_hours(Some(966));
        let (exec, _) = executor_with(vec![a, b], default_retry());

        assert_eq!(
            exec.read_usage_hours().await,
            Some(966),
            "chain-walk must fall through A's None to B's Some(966)"
        );
    }

    #[tokio::test]
    async fn read_usage_hours_none_when_no_controller_reports_it() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let (exec, _) = executor_with(vec![a], default_retry());
        assert_eq!(exec.read_usage_hours().await, None);
    }

    // ── T7 fix M1: panel_identity chain-walk ─────────────────────────────────

    #[tokio::test]
    async fn panel_identity_chain_walk_primary_none_fallback_some() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        // A has no panel identity (default None, mirroring a
        // `kwin-dpms`/`command` primary); B does (mirroring a `ddcci`
        // fallback that resolved its canonical key during probe).
        b.set_panel_identity(Some("i2c-dev:56 DEL DELL U2723QE".into()));
        let (exec, _) = executor_with(vec![a, b], default_retry());

        assert_eq!(
            exec.panel_identity().as_deref(),
            Some("i2c-dev:56 DEL DELL U2723QE"),
            "chain-walk must fall through A's None to B's Some(..)"
        );
    }

    #[tokio::test]
    async fn panel_identity_none_when_no_controller_reports_it() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let (exec, _) = executor_with(vec![a], default_retry());
        assert_eq!(exec.panel_identity(), None);
    }

    // ── Task 3: owner-first wake (RED-first probe; assertions extended post-impl) ──

    #[tokio::test]
    async fn wake_prefers_the_controller_that_successfully_blanked() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.set_available(false);
        b.push_blank_result(Ok(()));
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(1),
            "B (chain index 1) becomes owner after the successful blank"
        );

        a.set_available(true);
        exec.wake().await.unwrap();

        assert_eq!(b.count_op("wake"), 1, "B (owner) tried first");
        assert_eq!(a.count_op("wake"), 0, "A not tried since owner B succeeded");
        assert_eq!(
            exec.blank_owner_for_test(),
            None,
            "owner cleared once its own wake succeeds"
        );
    }

    #[tokio::test]
    async fn emergency_wake_once_uses_the_same_owner_first_order() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.set_available(false);
        b.push_blank_result(Ok(()));
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(1),
            "B (chain index 1) becomes owner after the successful blank"
        );

        a.set_available(true);
        exec.wake_once().await.unwrap();

        assert_eq!(b.count_op("wake"), 1, "B (owner) tried first");
        assert_eq!(a.count_op("wake"), 0, "A not tried since owner B succeeded");
        assert_eq!(
            exec.blank_owner_for_test(),
            None,
            "owner cleared once its own wake succeeds"
        );
    }

    #[tokio::test]
    async fn failed_new_blank_does_not_erase_the_previous_owner() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let (exec, _) = executor_with(vec![a.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(0),
            "sole controller becomes owner after a successful blank"
        );

        a.push_blank_result(Err(err("A")));
        let res = exec.blank(BlankMode::PowerOff).await;
        assert!(res.is_err(), "second blank fails (all controllers erred)");
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(0),
            "an all-failed blank must not erase the prior owner"
        );
    }

    #[tokio::test]
    async fn single_controller_chain_keeps_one_attempt_per_round() {
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let (exec, _) = executor_with(vec![a.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        exec.wake().await.unwrap();

        assert_eq!(
            a.count_op("blank"),
            1,
            "blank calls unchanged by owner-first logic"
        );
        assert_eq!(
            a.count_op("wake"),
            1,
            "wake calls unchanged by owner-first logic"
        );
    }

    // ── Task 3 fix-round Must 1: owner bypasses is_available() ──────────────

    #[tokio::test]
    async fn wake_attempts_unavailable_owner_first() {
        // a: available, blank ok, wake ok. b: available, blank ok, wake ok.
        // a's blank fails (one-shot script) so b becomes the blank owner.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.push_blank_result(Err(err("A")));
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(1),
            "B (chain index 1) becomes owner after the successful blank"
        );

        // Owner goes unavailable before wake — ownership must still win
        // over the `is_available()` gate.
        b.set_available(false);

        exec.wake().await.unwrap();

        assert_eq!(
            b.count_op("wake"),
            1,
            "owner B tried despite being unavailable"
        );
        assert_eq!(a.count_op("wake"), 0, "A not tried since owner B succeeded");
        assert_eq!(
            exec.blank_owner_for_test(),
            None,
            "owner cleared once its own wake succeeds"
        );
    }

    #[tokio::test]
    async fn wake_once_attempts_unavailable_owner_first() {
        // Same scenario as `wake_attempts_unavailable_owner_first` but
        // through the single-attempt `wake_once()` path.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        a.push_blank_result(Err(err("A")));
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(1),
            "B (chain index 1) becomes owner after the successful blank"
        );

        b.set_available(false);

        exec.wake_once().await.unwrap();

        assert_eq!(
            b.count_op("wake"),
            1,
            "owner B tried despite being unavailable"
        );
        assert_eq!(a.count_op("wake"), 0, "A not tried since owner B succeeded");
        assert_eq!(
            exec.blank_owner_for_test(),
            None,
            "owner cleared once its own wake succeeds"
        );
    }

    // ── Task 3 fix-round Must 2: non-owner fallback success retains owner ───

    #[tokio::test]
    async fn wake_retains_owner_when_owner_wake_fails_and_fallback_succeeds() {
        // A is the owner (sole controller when it blanks). A's wake fails;
        // B (non-owner fallback) succeeds. Per invariant, only the owner's
        // *own* successful wake clears ownership — a non-owner fallback
        // success must retain it.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(0),
            "A (chain index 0) becomes owner after the successful blank"
        );

        // default_retry has wake_retries: 0, so wake() runs exactly one
        // round — a single scripted owner-wake Err is enough.
        a.push_wake_result(Err(err("A")));

        exec.wake().await.unwrap();

        assert_eq!(a.count_op("wake"), 1, "owner A tried first and failed");
        assert_eq!(b.count_op("wake"), 1, "fallback B tried and succeeded");
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(0),
            "non-owner fallback success must NOT clear ownership"
        );
    }

    #[tokio::test]
    async fn wake_once_retains_owner_when_owner_wake_fails_and_fallback_succeeds() {
        // Same scenario as
        // `wake_retains_owner_when_owner_wake_fails_and_fallback_succeeds`
        // but through the single-attempt `wake_once()` path.
        let a = FakeController::new("A", vec![BlankMode::PowerOff]);
        let b = FakeController::new("B", vec![BlankMode::PowerOff]);
        let (exec, _) = executor_with(vec![a.clone(), b.clone()], default_retry());

        exec.blank(BlankMode::PowerOff).await.unwrap();
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(0),
            "A (chain index 0) becomes owner after the successful blank"
        );

        a.push_wake_result(Err(err("A")));

        exec.wake_once().await.unwrap();

        assert_eq!(a.count_op("wake"), 1, "owner A tried first and failed");
        assert_eq!(b.count_op("wake"), 1, "fallback B tried and succeeded");
        assert_eq!(
            exec.blank_owner_for_test(),
            Some(0),
            "non-owner fallback success must NOT clear ownership"
        );
    }

    // ── T5 Step 7 (P1, MANDATORY): trait-boundary priority proof ────────────

    /// Proves sampler priority is honored through the `CommandSink` trait
    /// boundary — the ONE place the rules engine / wear-tracking sampler
    /// ever talks to a display — using a real `DdcciController` + `FakeVcp`
    /// + real `PanelLock`, not a hand-rolled fake that could accidentally
    /// bypass the real acquisition logic.
    ///
    /// RED-first: this test is exactly what would fail if
    /// `DisplayExecutor::read_state_sampled` were deleted and the
    /// `CommandSink` trait default (delegate to `read_state`, command
    /// priority) took over instead — the sampled call would then block
    /// and succeed just like `read_state()`, and the `assert_eq!(..,
    /// None)` below would fail. Verified by temporarily deleting the
    /// override during development (see T5.md for the pasted failure).
    #[tokio::test]
    async fn read_state_sampled_priority_observable_at_command_sink_trait_boundary() {
        use std::time::Instant;

        let ident = "i2c-dev:99 TST TEST";
        let fake = Arc::new({
            let f = crate::vcp_ops::FakeVcp::new(vec![crate::vcp_ops::VcpDisplayInfo {
                ident_string: ident.into(),
            }]);
            f.expect_get(ident, 0xD6, Err("no".into())); // probe: D6 unsupported
            f
        });
        let locks = crate::ddc_lock::PanelLocks::new();
        let mut ddc = crate::ddcci::DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn crate::vcp_ops::VcpOps>,
            &locks,
        );
        ddc.probe().await.unwrap();
        let lock = ddc
            .panel_lock_for_test()
            .expect("a probed DdcciController has resolved a panel lock");

        // Wrap the real controller in the production CommandSink — the
        // executor — and erase it to `Arc<dyn CommandSink>`, exactly the
        // boundary the rules engine and the wear-tracking sampler see.
        let sink: Arc<dyn CommandSink> = Arc::new(DisplayExecutor::new(
            DisplayId("t5-panel".into()),
            vec![Box::new(ddc) as Box<dyn DisplayController>],
            BlankMode::BrightnessZero,
            default_retry(),
        ));

        // Two rounds of scripted responses. `FakeVcp::get_vcp` pops its
        // script entry before checking whether the priority gate lets the
        // transaction proceed (mirroring `RealVcp`, which can't know a
        // scripted value in advance either) — so the sampled attempt below
        // consumes one throwaway entry per code regardless of outcome; the
        // second, definitely-successful command-priority read consumes the
        // real entries.
        fake.expect_get(ident, 0x10, Ok(77)); // consumed by the sampled attempt
        fake.expect_get(ident, 0xD6, Err("no".into()));
        fake.expect_get(ident, 0x10, Ok(50)); // consumed by the command-priority read
        fake.expect_get(ident, 0xD6, Err("no".into()));

        // A "fake command" holds the panel lock on a REAL OS thread for
        // 60ms — standing in for an in-flight blank/wake elsewhere in the
        // daemon, exercising the real `PanelLock` mutex contention (not a
        // simulated announcement).
        let held_lock = Arc::clone(&lock);
        let holder = std::thread::spawn(move || {
            let _g = held_lock.command_guard();
            std::thread::sleep(Duration::from_millis(60));
        });
        // Let the holder thread actually acquire the mutex first.
        tokio::time::sleep(Duration::from_millis(15)).await;

        let sampled_start = Instant::now();
        let sampled = sink.read_state_sampled().await;
        let sampled_elapsed = sampled_start.elapsed();
        assert_eq!(
            sampled, None,
            "sampler must skip through the CommandSink trait boundary while \
             a command-path caller holds the panel lock"
        );
        assert!(
            sampled_elapsed < Duration::from_millis(30),
            "a skipped sampler read must return immediately, not block on \
             the held lock: {sampled_elapsed:?}"
        );

        // Command priority blocks until the holder releases, then
        // succeeds — the SAME scenario, proving both halves of the
        // priority contract at the same trait boundary.
        let start = Instant::now();
        let state = sink.read_state().await;
        let elapsed = start.elapsed();
        holder.join().unwrap();

        assert!(
            state.is_some(),
            "command-priority read_state must block then succeed: {state:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(20),
            "read_state must have actually waited for the held lock, not \
             raced past it: {elapsed:?}"
        );
    }

    // ── Task 6 test 6: one executor blank attempt == one DDC transaction ────

    /// Regression guard for the Task 6 post-write verification/rollback
    /// change (`DdcciController::verify_brightness_zero_write`): a single
    /// `DisplayExecutor::blank()` call against a chain of one `DdcciController`
    /// must start exactly ONE `set_vcp(0x10, 0)` transaction on the happy
    /// path (verification succeeds) — the executor's retry machinery (built
    /// for the wake burst; `blank()` has no retry loop at all) must never
    /// wrap or duplicate a blank write, and the new verification step must
    /// not either.
    ///
    /// "fork-internal write repeat is ONE transaction, not a controller
    /// retry" (Task 6 plan): the vendored `ddc-macos` fork's ARM write path
    /// performs two low-level I2C writes *inside* a single `execute_raw`
    /// call (see `vendor/ddc-macos/README.dormant.md`) — invisible above
    /// the `VcpOps::set_vcp` boundary this test observes. This test proves
    /// the layers ABOVE that boundary (executor, controller) never turn one
    /// logical write into more than one `set_vcp` call; it does not and
    /// cannot exercise the fork's internal write shape (macOS-only code,
    /// covered by the fork's own co-located tests, DEFERRED: PR CI).
    #[tokio::test]
    async fn one_executor_blank_attempt_starts_one_ddc_transaction() {
        let ident = "i2c-dev:99 TST TEST";
        let fake = Arc::new({
            let f = crate::vcp_ops::FakeVcp::new(vec![crate::vcp_ops::VcpDisplayInfo {
                ident_string: ident.into(),
            }]);
            f.expect_get(ident, 0xD6, Err("no".into())); // probe: D6 unsupported
            f
        });
        let locks = crate::ddc_lock::PanelLocks::new();
        let mut ddc = crate::ddcci::DdcciController::with_ops(
            None,
            80,
            BlankMode::BrightnessZero,
            Arc::clone(&fake) as Arc<dyn crate::vcp_ops::VcpOps>,
            &locks,
        );
        ddc.probe().await.unwrap();

        let sink: Arc<dyn CommandSink> = Arc::new(DisplayExecutor::new(
            DisplayId("t6-panel".into()),
            vec![Box::new(ddc) as Box<dyn DisplayController>],
            BlankMode::BrightnessZero,
            default_retry(),
        ));

        fake.expect_get(ident, 0x10, Ok(73)); // pre-write read
        fake.expect_set(ident, 0x10, 0, Ok(())); // the write
        fake.expect_get(ident, 0x10, Ok(0)); // post-write verification

        sink.blank(BlankMode::BrightnessZero).await.unwrap();

        let log = fake.take_call_log();
        let transaction_count = log
            .iter()
            .filter(|l| l.starts_with("set_vcp") && l.contains("0x10"))
            .count();
        assert_eq!(
            transaction_count, 1,
            "one executor blank attempt must start exactly one DDC \
             set_vcp(0x10, 0) transaction: {log:?}"
        );
    }
}
