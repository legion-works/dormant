//! Injectable per-panel DDC/CI lock (spec §4.3).
//!
//! ## Why a lock at all
//!
//! DDC/CI is a single I²C bus per physical panel: two concurrent VCP
//! transactions against the same panel corrupt each other (the 2026-07-09
//! probe measured a concurrent read spiking to 2370 ms against a daemon
//! `0xD6` write). Every physical VCP transaction — one `get_vcp` or one
//! `set_vcp` — must be serialized per panel. [`PanelLock`] is that
//! per-panel mutex; [`PanelLocks`] is the process-wide registry that hands
//! out the *same* [`PanelLock`] for the same panel identity, forever.
//!
//! ## Why `std::sync`, not `tokio::sync`
//!
//! The lock is acquired **inside** the `tokio::task::spawn_blocking`
//! closure that performs the physical transaction (see `vcp_ops.rs`), not
//! held across an `.await`. That closure runs on a blocking-pool thread
//! with no async runtime reachable from it — `tokio::sync::Mutex::lock()`
//! returns a future that would need to be `block_on`'d, which panics on a
//! current-thread runtime and is pointless ceremony on a multi-thread one.
//! A plain [`std::sync::Mutex`] blocks that OS thread directly, which is
//! exactly what a blocking-pool thread exists to do. Because the guard
//! never crosses an `.await` point, there is no `Send`-across-await-points
//! hazard either.
//!
//! ## Why injectable, not a `static`
//!
//! A `static PANEL_LOCKS` would share state across every test running in
//! the same process, breaking `cargo test`'s parallel-test isolation
//! (round-2 finding R2-M2): two unrelated tests using the same panel
//! identity string would contend on a lock neither test created. Instead
//! the daemon constructs one `Arc<PanelLocks>` in `App::start` — above
//! generation swaps, so an old-generation controller and a new-generation
//! controller for the same panel resolve to the *same* `Arc<PanelLock>` by
//! construction (closing the cross-generation bus race) — and threads it
//! through controller construction. Tests construct their own fresh
//! `PanelLocks::new()` and share nothing with any other test.
//!
//! ## Command priority (round-2 finding R2-M4)
//!
//! Two kinds of callers contend for a panel's lock:
//!
//! - **Command path** (blank/wake/exercise/seeding): user- or
//!   rules-engine-driven, latency-sensitive. Must never starve behind the
//!   sampler.
//! - **Sampler path** (periodic wear polling): best-effort, must yield
//!   instantly to any pending command rather than making it wait.
//!
//! A plain `std::sync::Mutex` is unfair — a `try_lock()` racing a blocked
//! `lock()` can win arbitrarily. [`PanelLock`] makes fairness explicit
//! state instead of relying on mutex implementation luck: `command_waiting`
//! is an `AtomicUsize` that every command-path caller increments *before*
//! blocking and decrements immediately on acquiring. The sampler's
//! [`PanelLock::sampler_try`] is a **double-check**: (i) a fast-path read
//! of `command_waiting` — skip immediately if nonzero; (ii) `try_lock()`;
//! (iii) re-read `command_waiting` — if a command announced itself in the
//! window between (i) and (ii) (the R3 TOCTOU), drop the guard immediately
//! and skip anyway. A single pre-check is not enough: a command can
//! increment `command_waiting` after the sampler's check but before its
//! `try_lock`, and an unfair mutex can then still hand the lock to the
//! sampler, making the command wait for a transaction that started *after*
//! it announced itself — exactly the bound this module exists to prevent
//! (invariant #1: a wake waits for at most one transaction already in
//! flight at its arrival).
//!
//! ## Poison recovery
//!
//! The lock guards a bus that is stateless between transactions — a
//! panicked transaction leaves nothing in the guarded `()` payload to
//! distrust. Every acquisition therefore recovers from poison
//! unconditionally (`unwrap_or_else(PoisonError::into_inner)` on the
//! blocking path, the equivalent match arm on `try_lock`) so that a single
//! scripted-panic VCP call can never wedge a panel's lock for the lifetime
//! of the process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

/// Process-wide (or test-local) registry of per-panel [`PanelLock`]s.
///
/// Deliberately **not** a `static`: the daemon owns exactly one
/// `Arc<PanelLocks>` for its whole lifetime (shared across generation
/// swaps so the same panel always resolves to the same lock), and each
/// test constructs its own fresh registry so parallel tests never
/// contend on locks they didn't create.
pub struct PanelLocks {
    inner: Mutex<HashMap<String, Arc<PanelLock>>>,
}

impl PanelLocks {
    /// Construct a fresh, empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Return the [`PanelLock`] for `key`, creating it on first use.
    ///
    /// The same `key` always resolves to the same `Arc<PanelLock>` for the
    /// lifetime of this registry — callers across generations that derive
    /// the same panel-identity key therefore serialize against each other
    /// by construction.
    #[must_use]
    pub fn get(&self, key: &str) -> Arc<PanelLock> {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            map.entry(key.to_string())
                .or_insert_with(|| Arc::new(PanelLock::new())),
        )
    }
}

/// A single panel's DDC/CI serialization lock.
///
/// Guards exactly one physical VCP transaction at a time. See the module
/// docs for the command-priority and poison-recovery rationale.
#[derive(Debug)]
pub struct PanelLock {
    mutex: Mutex<()>,
    command_waiting: AtomicUsize,
}

impl PanelLock {
    /// Construct a fresh, unlocked, zero-waiters lock.
    fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            command_waiting: AtomicUsize::new(0),
        }
    }

    /// Acquire the lock for a command-path operation (blank/wake/exercise/
    /// seeding): announce intent to waiting samplers, then block until the
    /// panel is free.
    ///
    /// Recovers from poison unconditionally — a panicked transaction never
    /// wedges the panel for later commands.
    #[must_use]
    pub fn command_guard(&self) -> PanelGuard<'_> {
        self.command_waiting.fetch_add(1, Ordering::SeqCst);
        let guard = self.mutex.lock().unwrap_or_else(PoisonError::into_inner);
        // The announcement covers the WAIT (spec §4.3.3); once acquired,
        // this caller is no longer a waiter.
        self.command_waiting.fetch_sub(1, Ordering::SeqCst);
        PanelGuard { _guard: guard }
    }

    /// Try to acquire the lock for a sampler-path operation (periodic wear
    /// polling). Returns `None` if a command-path caller is waiting or
    /// arrives mid-acquisition — the sampler must skip this tick rather
    /// than make a command wait for it.
    ///
    /// Double-checked per the module docs: a single pre-check cannot close
    /// the race where a command announces itself between the check and the
    /// `try_lock`. Delegates to `sampler_try_inner`, the single
    /// implementation of the `check→try_lock→recheck` sequence also exercised
    /// directly (via a race hook) by the `#[cfg(test)]`
    /// `sampler_try_with_race` — so the pin test for the R3
    /// TOCTOU close runs this exact production control flow, not a
    /// hand-duplicated copy of it.
    #[must_use]
    pub fn sampler_try(&self) -> Option<PanelGuard<'_>> {
        self.sampler_try_inner(|_stage: &'static str| {})
    }

    /// Shared core of [`PanelLock::sampler_try`]: (i) fast-path check, (ii)
    /// `try_lock`, (iii) re-check — with an `on_stage` hook invoked at each
    /// phase boundary (`"check"`, `"race"`, `"try_lock"`, `"recheck"`).
    ///
    /// In the production build (called only from [`PanelLock::sampler_try`]
    /// with a no-op closure) the hook is inlined away — `impl FnMut` is
    /// monomorphized per call site, so an empty closure body compiles to no
    /// extra work. In test builds, `sampler_try_with_race`
    /// passes a hook that both records the stage log and, at the `"race"`
    /// stage — strictly between (i) and (ii) — runs an injected closure to
    /// provoke the R3 TOCTOU. This is the one and only implementation of the
    /// algorithm; there is no second copy for tests to accidentally diverge
    /// from.
    #[cfg_attr(not(test), inline)]
    fn sampler_try_inner(&self, mut on_stage: impl FnMut(&'static str)) -> Option<PanelGuard<'_>> {
        // (i) fast-path check.
        on_stage("check");
        if self.command_waiting.load(Ordering::SeqCst) > 0 {
            return None;
        }
        // Race window: strictly between (i) and (ii).
        on_stage("race");
        // (ii) try to acquire.
        on_stage("try_lock");
        let guard = match self.mutex.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        // (iii) re-check: a command may have announced itself in the
        // window between (i) and (ii).
        on_stage("recheck");
        if self.command_waiting.load(Ordering::SeqCst) > 0 {
            drop(guard);
            return None;
        }
        Some(PanelGuard { _guard: guard })
    }

    /// Test-only hook proving the race-window placement of
    /// [`PanelLock::sampler_try`]'s double-check: `f` runs strictly between
    /// the fast-path check (i) and the `try_lock` (ii), and the returned
    /// stage log records each phase so a test can assert the closure ran
    /// where the design claims it does, not merely that the final outcome
    /// looks right. Calls `sampler_try_inner` — the same
    /// function `sampler_try` calls — so this exercises production's real
    /// control flow rather than a parallel reimplementation.
    #[cfg(test)]
    fn sampler_try_with_race(
        &self,
        f: impl FnOnce(),
    ) -> (Option<PanelGuard<'_>>, Vec<&'static str>) {
        let mut stages = Vec::new();
        let mut f = Some(f);
        let result = self.sampler_try_inner(|stage| {
            stages.push(stage);
            if stage == "race"
                && let Some(f) = f.take()
            {
                f();
            }
        });
        (result, stages)
    }

    /// Test-only: announce a command-path waiter without blocking on the
    /// mutex, for provoking the sampler's double-check race directly.
    #[cfg(test)]
    fn announce_command_for_test(&self) {
        self.command_waiting.fetch_add(1, Ordering::SeqCst);
    }

    /// Test-only: retract an announcement made via
    /// [`PanelLock::announce_command_for_test`].
    #[cfg(test)]
    fn retract_command_for_test(&self) {
        self.command_waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

/// RAII guard held for the duration of one physical VCP transaction.
///
/// Carries no data of its own — the guarded value is `()`, since the lock
/// exists purely to serialize access, not to protect shared state.
pub struct PanelGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn same_key_same_lock_instance() {
        // spec pin (e) unit form
        let locks = PanelLocks::new();
        let a = locks.get("ddc:aoc:1");
        let b = locks.get("ddc:aoc:1");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &locks.get("ddc:other:2")));
    }

    #[test]
    fn command_guard_serializes() {
        // pin (a) unit form
        let locks = PanelLocks::new();
        let l = locks.get("p");
        let l2 = Arc::clone(&l);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let o2 = Arc::clone(&order);
        let g = l.command_guard();
        let h = thread::spawn(move || {
            let _g = l2.command_guard();
            o2.lock().unwrap().push("second");
        });
        thread::sleep(Duration::from_millis(50));
        order.lock().unwrap().push("first");
        drop(g);
        h.join().unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["first", "second"]);
    }

    #[test]
    fn sampler_skips_when_command_waiting() {
        // pin (b) — realistic scenario: a command-path caller is genuinely
        // blocked on the mutex while announced. Kept alongside the
        // de-confounded `sampler_fast_path_skips_without_lock_contention`
        // below because it still exercises the end-to-end interaction
        // between `command_guard` and `sampler_try` under real contention,
        // which the isolated test does not.
        let locks = PanelLocks::new();
        let l = locks.get("p");
        let g = l.command_guard(); // holds lock
        let l2 = Arc::clone(&l);
        let waiter = thread::spawn(move || {
            let _g = l2.command_guard();
        }); // blocked, announced
        thread::sleep(Duration::from_millis(50)); // waiter has incremented
        assert!(l.sampler_try().is_none()); // skip: command_waiting > 0 and/or mutex held
        drop(g);
        waiter.join().unwrap();
    }

    #[test]
    fn sampler_fast_path_skips_without_lock_contention() {
        // pin (b), de-confounded (T4 review M-2). No thread ever touches
        // the mutex here, so try_lock() would trivially succeed if step (i)
        // were absent — and merely asserting `is_none()` would NOT isolate
        // step (i) from step (iii)'s recheck either: `command_waiting`
        // stays nonzero for the whole synchronous call either way, so the
        // (iii) recheck would also yield `None` even with (i) deleted,
        // masking the mutation. The stage log — recorded by the very same
        // `sampler_try_inner` that `sampler_try` calls — proves the skip
        // happens AT the fast-path check itself: execution never reaches
        // "try_lock"/"recheck" at all.
        let locks = PanelLocks::new();
        let l = locks.get("p");
        l.announce_command_for_test(); // no thread blocked; mutex free
        let mut stages = Vec::new();
        let got = l.sampler_try_inner(|stage| stages.push(stage));
        assert!(
            got.is_none(),
            "fast-path check (i) must skip on announcement alone, with the mutex uncontended"
        );
        assert_eq!(
            stages,
            vec!["check"],
            "must return at the fast-path check, never reaching try_lock/recheck"
        );
        l.retract_command_for_test();
        assert!(
            l.sampler_try().is_some(),
            "sampler proceeds once the announcement is retracted"
        );
    }

    #[test]
    fn sampler_double_check_yields_to_late_command() {
        // pin (f) — the R3 TOCTOU
        let locks = PanelLocks::new();
        let l = locks.get("p");
        let l2 = Arc::clone(&l);
        let (got, stages) = l.sampler_try_with_race(move || {
            l2.announce_command_for_test(); // announce mid-race, non-blocking
        });
        assert_eq!(
            stages,
            vec!["check", "race", "try_lock", "recheck"],
            "race closure must run between fast-path check and try_lock"
        );
        assert!(
            got.is_none(),
            "sampler must release and skip when a command announced mid-acquisition"
        );
        l.retract_command_for_test();
        // (P5) positive follow-on: the counter is not stuck — sampler proceeds again
        assert!(
            l.sampler_try().is_some(),
            "command_waiting must return to zero; sampler proceeds"
        );
    }

    #[test]
    fn poison_recovered_after_panic() {
        // pin (d) unit form
        let locks = PanelLocks::new();
        let l = locks.get("p");
        let l2 = Arc::clone(&l);
        let _ = thread::spawn(move || {
            let _g = l2.command_guard();
            panic!("vcp exploded");
        })
        .join();
        // poisoned now — both paths must still acquire
        let g = l.command_guard();
        drop(g);
        assert!(l.sampler_try().is_some());
    }

    #[test]
    fn fresh_registries_share_nothing() {
        // the R2-M2 test-isolation property
        let a = PanelLocks::new();
        let b = PanelLocks::new();
        let ga = a.get("k");
        let _held = ga.command_guard();
        assert!(
            b.get("k").sampler_try().is_some(),
            "separate registry, separate lock"
        );
    }
}
