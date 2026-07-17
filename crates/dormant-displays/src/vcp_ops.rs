//! Abstract DDC/CI operations (`VcpOps` trait), a real implementation backed by
//! ddc-hi (wrapped in `spawn_blocking`), and a scripted fake for unit tests.
//!
//! ## Design
//!
//! Every hardware touch is wrapped in [`tokio::task::spawn_blocking`] so the
//! async executor is never blocked. No ddc-hi `Display` handle is cached across
//! operations — each call re-enumerates, finds the matching display, performs
//! the op, and drops the handle. Enumeration is ~100 ms, which is acceptable
//! at blank/wake frequency.
//!
//! The trait is `#[async_trait]` so that the real implementation can `await`
//! the `spawn_blocking` join handle directly, avoiding the
//! `block_in_place`/`block_on` triple-wrap that panics on current-thread
//! runtimes.
//!
//! ## Panel-lock discipline (spec §4.3, §9#1)
//!
//! Every physical VCP transaction (one `get_vcp` / `set_vcp` / `get_vcp_raw`
//! call) is serialized per panel through a [`crate::ddc_lock::PanelLock`],
//! acquired **inside** the `spawn_blocking` closure that performs the
//! transaction — never held across an `.await`. [`VcpPriority`] selects
//! which acquisition strategy that closure uses: [`VcpPriority::Command`]
//! blocks until the panel is free (command/blank/wake/exercise/seeding
//! callers, which must never starve); [`VcpPriority::Sampler`] tries once
//! and yields immediately to any command-path caller (periodic wear
//! polling, which must never make a command wait for it).
//!
//! A scripted or real ddc-hi panic during the physical op is caught with
//! `std::panic::catch_unwind` *inside* the guarded section (guard acquired
//! first, `catch_unwind` wraps only the op) and converted to the sentinel
//! error [`VCP_PANIC`] — a single bad transaction can never crash the
//! `spawn_blocking` worker thread or poison the panel lock's usability for
//! the next caller (`PanelLock`'s own poison recovery is defense in depth
//! for other unwind paths).
//!
//! When [`VcpPriority::Sampler`] loses the race for the lock, the call
//! returns the sentinel error [`VCP_SKIPPED`] — never surfaced as a
//! display failure, only ever collapsed to `None` by the sampled read
//! path (mirroring how a real read error is already collapsed to `None`
//! by [`crate::ddcci::DdcciController`]'s
//! [`read_state`](dormant_core::traits::DisplayController::read_state)).

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ddc_hi::Ddc;

use crate::ddc_lock::PanelLock;

/// Which acquisition strategy a VCP call's panel-lock guard uses.
///
/// See the module docs for the fairness rationale — this is threaded from
/// the calling controller (command path vs. sampler path), never inferred
/// from the operation itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpPriority {
    /// Command path: blank/wake/exercise/seeding. Blocks until the panel is
    /// free; never starved behind the sampler.
    Command,
    /// Sampler path: periodic wear polling. Tries once; yields immediately
    /// to any command-path caller.
    Sampler,
}

/// Sentinel error returned when a [`VcpPriority::Sampler`] call loses the
/// race for the panel lock. Never a real hardware failure — callers on the
/// sampled path collapse this (like any other error) to `None`.
pub const VCP_SKIPPED: &str = "VCP_SKIPPED: sampler yielded to a command-path caller";

/// Sentinel error returned when the wrapped ddc-hi operation panics.
/// `catch_unwind` converts the panic into this string so a single bad
/// transaction cannot crash the `spawn_blocking` worker thread.
pub const VCP_PANIC: &str = "E_DISPLAY_IO: vcp operation panicked";

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
/// Methods are async so the real implementation can `await` the blocking
/// task directly without `block_in_place`/`block_on` gymnastics.
#[async_trait]
pub trait VcpOps: Send + Sync {
    /// Enumerate all DDC/CI-capable displays.
    async fn list_displays(&self) -> Vec<VcpDisplayInfo>;

    /// Get the current value of a VCP feature code.
    ///
    /// `lock` is the caller's panel lock (derived from its canonical
    /// identity — see `ddcci::DdcciController::probe`); `prio` selects the
    /// acquisition strategy (see [`VcpPriority`]). The lock is acquired
    /// **inside** the implementation's `spawn_blocking` closure, never held
    /// across this `.await`.
    ///
    /// # Errors
    ///
    /// Returns an error string if the VCP read fails (I/O error, display
    /// disconnected, unsupported feature code), if the operation panicked
    /// ([`VCP_PANIC`]), or if a [`VcpPriority::Sampler`] call lost the race
    /// for the lock ([`VCP_SKIPPED`]).
    async fn get_vcp(
        &self,
        ident: &str,
        code: u8,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<u16, String>;

    /// Set a VCP feature code to a value. See [`Self::get_vcp`] for the
    /// `lock` / `prio` contract.
    ///
    /// # Errors
    ///
    /// Returns an error string if the VCP write fails (I/O error, display
    /// disconnected, or unsupported feature code), if the operation
    /// panicked ([`VCP_PANIC`]), or if a [`VcpPriority::Sampler`] call lost
    /// the race for the lock ([`VCP_SKIPPED`]).
    async fn set_vcp(
        &self,
        ident: &str,
        code: u8,
        value: u16,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<(), String>;

    /// Get the raw 4-byte VCP reply `[mh, ml, sh, sl]` for a feature code.
    ///
    /// Needed for VCP `0xC0` (usage hours): the value can exceed `u16`'s
    /// range (`get_vcp`'s return type), so the full max/min-high/low byte
    /// quadruple is returned uninterpreted — the caller decodes per its own
    /// feature-specific rules (see
    /// `ddcci::DdcciController::read_usage_hours`). See [`Self::get_vcp`]
    /// for the `lock` / `prio` contract.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::get_vcp`].
    async fn get_vcp_raw(
        &self,
        ident: &str,
        code: u8,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<[u8; 4], String>;
}

/// Acquire `lock` per `prio`, inside the blocking closure. Returns
/// [`VCP_SKIPPED`] as an `Err` when a [`VcpPriority::Sampler`] call loses
/// the race — the one shared acquisition policy every `VcpOps` method (real
/// or fake) uses, so the priority contract has a single implementation.
fn acquire(lock: &PanelLock, prio: VcpPriority) -> Result<crate::ddc_lock::PanelGuard<'_>, String> {
    match prio {
        VcpPriority::Command => Ok(lock.command_guard()),
        VcpPriority::Sampler => lock.sampler_try().ok_or_else(|| VCP_SKIPPED.to_string()),
    }
}

// ── RealVcp — wraps ddc-hi in spawn_blocking ───────────────────────────────────

/// Real DDC/CI operations backed by ddc-hi, with every call wrapped in
/// [`tokio::task::spawn_blocking`].
///
/// Available on Linux (I²C-dev) and macOS (the vendored, path-patched
/// `ddc-macos` fork — see `vendor/ddc-macos/README.dormant.md`). On both
/// platforms `ddc_hi::Display` hides the backend behind one enum `Handle`,
/// so every method below is identical code for both — there is no
/// macOS-specific branch here. A macOS host with the private `CoreDisplay`
/// symbols unavailable (`ddc-macos`'s `Error::MissingCoreDisplaySymbol` /
/// `CoreDisplayFrameworkUnavailable`) never surfaces as a panic or a build
/// failure: `ddc_macos::Monitor::enumerate()` treats a display it can't
/// resolve a service for as absent rather than erroring the whole
/// enumeration (upstream `ddc-hi` only trusts an `Ok` enumeration result
/// too), so it simply drops out of `list_displays()` — the existing empty/
/// no-match handling in `DdcciController::probe`/`is_available` already
/// turns that into an ordinary "unavailable" outcome.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct RealVcp;

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[async_trait]
impl VcpOps for RealVcp {
    async fn list_displays(&self) -> Vec<VcpDisplayInfo> {
        let displays = tokio::task::spawn_blocking(ddc_hi::Display::enumerate)
            .await
            .unwrap_or_default();
        displays
            .into_iter()
            .map(|d| VcpDisplayInfo {
                ident_string: d.info.to_string(),
            })
            .collect()
    }

    async fn get_vcp(
        &self,
        ident: &str,
        code: u8,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<u16, String> {
        let ident = ident.to_string();
        let lock = Arc::clone(lock);
        tokio::task::spawn_blocking(move || {
            // Panel-lock guard acquired FIRST, outside `catch_unwind` — a
            // panic in the wrapped op therefore never poisons the mutex
            // (the guard is dropped normally when this closure returns,
            // not via unwinding through its scope).
            let _guard = acquire(&lock, prio)?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut displays = Self::enumerate_displays();
                let display = Self::find_display(&ident, &mut displays)?;
                let vcp = display
                    .handle
                    .get_vcp_feature(code)
                    .map_err(|e| format!("get_vcp(0x{code:02X}) failed: {e}"))?;
                Ok::<u16, String>(vcp.value())
            }))
            .unwrap_or_else(|_| Err(VCP_PANIC.to_string()))
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }

    async fn set_vcp(
        &self,
        ident: &str,
        code: u8,
        value: u16,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<(), String> {
        let ident = ident.to_string();
        let lock = Arc::clone(lock);
        tokio::task::spawn_blocking(move || {
            let _guard = acquire(&lock, prio)?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut displays = Self::enumerate_displays();
                let display = Self::find_display(&ident, &mut displays)?;
                display
                    .handle
                    .set_vcp_feature(code, value)
                    .map_err(|e| format!("set_vcp(0x{code:02X}, {value}) failed: {e}"))
            }))
            .unwrap_or_else(|_| Err(VCP_PANIC.to_string()))
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }

    async fn get_vcp_raw(
        &self,
        ident: &str,
        code: u8,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<[u8; 4], String> {
        let ident = ident.to_string();
        let lock = Arc::clone(lock);
        tokio::task::spawn_blocking(move || {
            let _guard = acquire(&lock, prio)?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut displays = Self::enumerate_displays();
                let display = Self::find_display(&ident, &mut displays)?;
                let vcp = display
                    .handle
                    .get_vcp_feature(code)
                    .map_err(|e| format!("get_vcp_raw(0x{code:02X}) failed: {e}"))?;
                Ok::<[u8; 4], String>([vcp.mh, vcp.ml, vcp.sh, vcp.sl])
            }))
            .unwrap_or_else(|_| Err(VCP_PANIC.to_string()))
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }
}

// ── FakeVcp — scripted operations for tests ────────────────────────────────────

/// A scripted [`VcpOps`] implementation for unit tests.
///
/// Each call records its arguments in a call log (accessible via
/// `take_call_log`) and returns values from a pre-configured script.
/// All mutable state is behind [`StdMutex`] so the trait's `&self` methods
/// can mutate script state and the call log.
///
/// This type is `pub(crate)` and only used by the `ddcci` test module, so
/// it is dead code in non-test builds.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct FakeVcp {
    displays: Vec<VcpDisplayInfo>,
    /// (ident, code) → Result<value, err>
    get_script: StdMutex<Vec<ScriptEntry>>,
    /// (ident, code) → Result<[mh, ml, sh, sl], err>
    get_raw_script: StdMutex<Vec<RawScriptEntry>>,
    /// (ident, code, value) → Result<(), err>
    set_script: StdMutex<Vec<SetScriptEntry>>,
    /// (ident, code) → a fixed delay applied *while the panel lock is
    /// held*, for the "slow read delays wake by that read only" pin test.
    get_delay: StdMutex<std::collections::HashMap<(String, u8), std::time::Duration>>,
    /// (ident, code) idents scripted to panic (instead of returning their
    /// scripted result) on their next `get_vcp` call — one-shot, removed on
    /// use.
    get_panic: StdMutex<std::collections::HashSet<(String, u8)>>,
    /// (ident, code, value) triples scripted to panic on their next
    /// `set_vcp` call — one-shot, removed on use.
    set_panic: StdMutex<std::collections::HashSet<(String, u8, u16)>>,
    call_log: StdMutex<Vec<String>>,
    /// Wall-clock elapsed time of the most recent delayed `get_vcp` call
    /// (measured *inside* the blocking closure, while the lock was held) —
    /// the ground truth the relational wake-latency pin test compares
    /// against.
    last_get_elapsed: StdMutex<Option<std::time::Duration>>,
}

/// A single scripted `get_vcp` response.
#[cfg_attr(not(test), allow(dead_code))]
type ScriptEntry = ((String, u8), Result<u16, String>);

/// A single scripted `get_vcp_raw` response.
#[cfg_attr(not(test), allow(dead_code))]
type RawScriptEntry = ((String, u8), Result<[u8; 4], String>);

/// A single scripted `set_vcp` response.
#[cfg_attr(not(test), allow(dead_code))]
type SetScriptEntry = ((String, u8, u16), Result<(), String>);

#[cfg_attr(not(test), allow(dead_code))]
impl FakeVcp {
    /// Create a new `FakeVcp` with the given displays.
    #[must_use]
    pub fn new(displays: Vec<VcpDisplayInfo>) -> Self {
        Self {
            displays,
            get_script: StdMutex::new(Vec::new()),
            get_raw_script: StdMutex::new(Vec::new()),
            set_script: StdMutex::new(Vec::new()),
            get_delay: StdMutex::new(std::collections::HashMap::new()),
            get_panic: StdMutex::new(std::collections::HashSet::new()),
            set_panic: StdMutex::new(std::collections::HashSet::new()),
            call_log: StdMutex::new(Vec::new()),
            last_get_elapsed: StdMutex::new(None),
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

    /// Add a scripted `get_vcp_raw` response.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_get_raw(&self, ident: &str, code: u8, result: Result<[u8; 4], String>) {
        self.get_raw_script
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

    /// The next `get_vcp(ident, code)` call sleeps `delay` *while holding
    /// the panel lock* before returning its scripted result — used by the
    /// relational "slow read delays wake by that read only" pin test.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_get_delay(&self, ident: &str, code: u8, delay: std::time::Duration) {
        self.get_delay
            .lock()
            .unwrap()
            .insert((ident.to_string(), code), delay);
    }

    /// The next `get_vcp(ident, code)` call panics instead of returning a
    /// scripted result (one-shot) — used by the scripted-panic pin test.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_get_panic(&self, ident: &str, code: u8) {
        self.get_panic
            .lock()
            .unwrap()
            .insert((ident.to_string(), code));
    }

    /// The next `set_vcp(ident, code, value)` call panics instead of
    /// returning a scripted result (one-shot) — used by the scripted-panic
    /// pin test.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_set_panic(&self, ident: &str, code: u8, value: u16) {
        self.set_panic
            .lock()
            .unwrap()
            .insert((ident.to_string(), code, value));
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

    /// Take the elapsed wall-clock time of the most recent delayed
    /// `get_vcp` call, if any.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn take_last_get_elapsed(&self) -> Option<std::time::Duration> {
        self.last_get_elapsed.lock().unwrap().take()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[async_trait]
impl VcpOps for FakeVcp {
    async fn list_displays(&self) -> Vec<VcpDisplayInfo> {
        self.displays.clone()
    }

    async fn get_vcp(
        &self,
        ident: &str,
        code: u8,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<u16, String> {
        self.call_log
            .lock()
            .unwrap()
            .push(format!("get_vcp({ident}, 0x{code:02X})"));
        let key = (ident.to_string(), code);
        let scripted = {
            let mut script = self.get_script.lock().unwrap();
            let idx = script
                .iter()
                .position(|((id, c), _)| *id == key.0 && *c == key.1);
            match idx {
                Some(i) => script.remove(i).1,
                None => Err(format!(
                    "FakeVcp: no scripted response for get_vcp({ident}, 0x{code:02X})"
                )),
            }
        };
        let delay = self.get_delay.lock().unwrap().get(&key).copied();
        let should_panic = self.get_panic.lock().unwrap().remove(&key);
        let lock = Arc::clone(lock);

        let (result, elapsed) = tokio::task::spawn_blocking(move || {
            // Panel-lock guard acquired FIRST, exactly like `RealVcp` — the
            // "canonical key derived → lock held → first transaction"
            // ordering and the sampler-skip contract both run through this
            // same acquisition point in tests as in production.
            let _guard = match acquire(&lock, prio) {
                Ok(g) => g,
                Err(e) => return (Err(e), std::time::Duration::ZERO),
            };
            let start = std::time::Instant::now();
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(d) = delay {
                    std::thread::sleep(d);
                }
                assert!(
                    !should_panic,
                    "FakeVcp: scripted panic for get_vcp(0x{code:02X})"
                );
                scripted
            }))
            .unwrap_or_else(|_| Err(VCP_PANIC.to_string()));
            (out, start.elapsed())
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?;

        if !elapsed.is_zero() {
            *self.last_get_elapsed.lock().unwrap() = Some(elapsed);
        }
        result
    }

    async fn set_vcp(
        &self,
        ident: &str,
        code: u8,
        value: u16,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<(), String> {
        self.call_log
            .lock()
            .unwrap()
            .push(format!("set_vcp({ident}, 0x{code:02X}, {value})"));
        let key = (ident.to_string(), code, value);
        let scripted = {
            let mut script = self.set_script.lock().unwrap();
            let idx = script
                .iter()
                .position(|((id, c, v), _)| *id == key.0 && *c == key.1 && *v == key.2);
            match idx {
                Some(i) => script.remove(i).1,
                None => Err(format!(
                    "FakeVcp: no scripted response for set_vcp({ident}, 0x{code:02X}, {value})"
                )),
            }
        };
        let should_panic = self.set_panic.lock().unwrap().remove(&key);
        let lock = Arc::clone(lock);

        tokio::task::spawn_blocking(move || {
            let _guard = acquire(&lock, prio)?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                assert!(
                    !should_panic,
                    "FakeVcp: scripted panic for set_vcp(0x{code:02X}, {value})"
                );
                scripted
            }))
            .unwrap_or_else(|_| Err(VCP_PANIC.to_string()))
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }

    async fn get_vcp_raw(
        &self,
        ident: &str,
        code: u8,
        lock: &Arc<PanelLock>,
        prio: VcpPriority,
    ) -> Result<[u8; 4], String> {
        self.call_log
            .lock()
            .unwrap()
            .push(format!("get_vcp_raw({ident}, 0x{code:02X})"));
        let key = (ident.to_string(), code);
        let scripted = {
            let mut script = self.get_raw_script.lock().unwrap();
            let idx = script
                .iter()
                .position(|((id, c), _)| *id == key.0 && *c == key.1);
            match idx {
                Some(i) => script.remove(i).1,
                None => Err(format!(
                    "FakeVcp: no scripted response for get_vcp_raw({ident}, 0x{code:02X})"
                )),
            }
        };
        let lock = Arc::clone(lock);

        tokio::task::spawn_blocking(move || {
            let _guard = acquire(&lock, prio)?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scripted))
                .unwrap_or_else(|_| Err(VCP_PANIC.to_string()))
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::ddc_lock::PanelLocks;

    fn single_display_fake() -> FakeVcp {
        FakeVcp::new(vec![VcpDisplayInfo {
            ident_string: "i2c-dev:1 TST TEST".into(),
        }])
    }

    const IDENT: &str = "i2c-dev:1 TST TEST";

    #[tokio::test]
    async fn get_vcp_returns_scripted_value() {
        let fake = single_display_fake();
        let locks = PanelLocks::new();
        let lock = locks.get(IDENT);
        fake.expect_get(IDENT, 0x10, Ok(42));

        let v = fake
            .get_vcp(IDENT, 0x10, &lock, VcpPriority::Command)
            .await
            .unwrap();
        assert_eq!(v, 42);
    }

    /// T5 pin test 3 (mechanics half): `get_vcp_raw` carries the full
    /// 4-byte reply uninterpreted — `ddcci::decode_usage_hours` does the
    /// feature-specific decode.
    #[tokio::test]
    async fn get_vcp_raw_returns_scripted_bytes() {
        let fake = single_display_fake();
        let locks = PanelLocks::new();
        let lock = locks.get(IDENT);
        fake.expect_get_raw(IDENT, 0xC0, Ok([0x00, 0x00, 0x03, 0xC6]));

        let raw = fake
            .get_vcp_raw(IDENT, 0xC0, &lock, VcpPriority::Command)
            .await
            .unwrap();
        assert_eq!(raw, [0x00, 0x00, 0x03, 0xC6]);
    }

    /// T5 pin test 6(a) (mechanics half): two command-priority
    /// transactions against the SAME panel lock never overlap — the
    /// second one's op only starts after the first one's guard is
    /// dropped. Proven by an elapsed-time bound, not a bare sleep-and-hope:
    /// if the two ran concurrently, `total` would be ≈ the longer of the
    /// two delays rather than their sum.
    #[tokio::test]
    async fn command_priority_serializes_two_concurrent_transactions() {
        let fake = Arc::new(single_display_fake());
        let locks = PanelLocks::new();
        let lock = locks.get(IDENT);

        fake.expect_get_delay(IDENT, 0x10, Duration::from_millis(60));
        fake.expect_get(IDENT, 0x10, Ok(1));
        fake.expect_get(IDENT, 0x11, Ok(2));

        let fake1 = Arc::clone(&fake);
        let task_lock = Arc::clone(&lock);
        let start = Instant::now();
        let t1 = tokio::spawn(async move {
            fake1
                .get_vcp(IDENT, 0x10, &task_lock, VcpPriority::Command)
                .await
        });
        // Give t1 a chance to acquire the lock and enter its 60ms delay
        // before t2 arrives.
        tokio::time::sleep(Duration::from_millis(15)).await;
        let t2_start = Instant::now();
        let v2 = fake
            .get_vcp(IDENT, 0x11, &lock, VcpPriority::Command)
            .await
            .unwrap();
        let t2_elapsed = t2_start.elapsed();
        t1.await.unwrap().unwrap();
        let total = start.elapsed();

        assert_eq!(v2, 2);
        assert!(
            t2_elapsed >= Duration::from_millis(40),
            "t2 (0x11) must have waited for t1's (0x10) in-flight transaction \
             to release the shared panel lock; t2_elapsed={t2_elapsed:?}"
        );
        assert!(
            total >= Duration::from_millis(60),
            "combined wall-clock must be at least the full 60ms delay — \
             proves the two transactions were serialized, not concurrent: {total:?}"
        );
    }

    /// T5 pin test: sampler priority yields immediately when the panel
    /// lock is already held by a command-path caller — never surfaced as
    /// a display failure (the sentinel [`VCP_SKIPPED`] is what the sampled
    /// read path collapses to `None`, exactly like any other read error).
    #[tokio::test]
    async fn sampler_priority_skips_when_lock_is_held() {
        let locks = PanelLocks::new();
        let lock = locks.get(IDENT);
        // Uncontended acquisition — held for the duration of the test, not
        // across any `.await` of a SPAWNED task, so no `Send` hazard.
        let held = lock.command_guard();

        let fake = single_display_fake();
        let err = fake
            .get_vcp(IDENT, 0x10, &lock, VcpPriority::Sampler)
            .await
            .unwrap_err();

        assert_eq!(
            err, VCP_SKIPPED,
            "sampler must skip via the VCP_SKIPPED sentinel"
        );
        drop(held);
    }

    /// T5 pin test 6(d) (mechanics half): a scripted panic inside the
    /// wrapped op is caught and converted to [`VCP_PANIC`] — it must never
    /// crash the calling task, and the panel lock must still be usable for
    /// the very next call.
    #[tokio::test]
    async fn scripted_panic_converts_to_err_and_lock_remains_usable() {
        let fake = single_display_fake();
        let locks = PanelLocks::new();
        let lock = locks.get(IDENT);
        fake.expect_get_panic(IDENT, 0x10);

        let err = fake
            .get_vcp(IDENT, 0x10, &lock, VcpPriority::Command)
            .await
            .unwrap_err();
        assert_eq!(err, VCP_PANIC);

        // Lock still usable — the next transaction succeeds normally.
        fake.expect_get(IDENT, 0x10, Ok(7));
        let v = fake
            .get_vcp(IDENT, 0x10, &lock, VcpPriority::Command)
            .await
            .unwrap();
        assert_eq!(v, 7);
    }

    /// Task 6 test 4: sampler yields to a waiting command through the
    /// `vcp_ops` acquisition layer (`acquire()` + `PanelLock`), combining
    /// the existing delayed-transaction pattern
    /// (`command_priority_serializes_two_concurrent_transactions`) with the
    /// existing skip-on-contention pattern (`sampler_priority_skips_when_lock_is_held`)
    /// into one three-party scenario neither covers alone: an in-flight
    /// sampler read, a command that arrives and announces itself while that
    /// read is still running, and a SECOND sampler read that arrives after
    /// the command has announced.
    ///
    /// `FakeVcp::call_log` records each call the instant its (synchronous)
    /// prelude runs — before the spawned closure ever touches the panel
    /// lock — so, given the staggered spawn order below, the log order IS
    /// invocation order: `["sample-1", "command", "sample-2"]`. That both
    /// proves the plain ordering AND, combined with the return-value
    /// assertions, proves `sample-2` never blocked behind `sample-1`'s
    /// in-flight transaction — it yielded (`VCP_SKIPPED`) the instant it
    /// observed the command's announcement, never touching the panel.
    #[tokio::test]
    async fn sampler_yields_to_a_waiting_command_through_real_vcp_ops() {
        let fake = Arc::new(single_display_fake());
        let panel_locks = PanelLocks::new();
        let panel_lock = panel_locks.get(IDENT);

        // sample-1: holds the panel for 80ms via a sampler-priority read on
        // 0x10 — the "in-flight sampler transaction" a command must wait
        // behind (mirrors `command_priority_serializes_two_concurrent_transactions`).
        fake.expect_get_delay(IDENT, 0x10, Duration::from_millis(80));
        fake.expect_get(IDENT, 0x10, Ok(1));
        // command: a command-priority read on a distinct code (0x11) so its
        // call-log entry is unambiguous; it must wait for sample-1 to
        // release before it can even attempt acquisition.
        fake.expect_get(IDENT, 0x11, Ok(2));
        // sample-2 (0x12) is deliberately left unscripted: it must yield
        // via VCP_SKIPPED before ever consuming a script entry — if it
        // instead blocked and eventually ran, it would surface FakeVcp's
        // "no scripted response" error instead, failing the assertion below
        // for the right reason.

        let fake1 = Arc::clone(&fake);
        let sample1_lock = Arc::clone(&panel_lock);
        let sample1 = tokio::spawn(async move {
            fake1
                .get_vcp(IDENT, 0x10, &sample1_lock, VcpPriority::Sampler)
                .await
        });

        // Let sample-1 acquire the lock and enter its 80ms delay.
        tokio::time::sleep(Duration::from_millis(15)).await;

        let fake2 = Arc::clone(&fake);
        let command_lock = Arc::clone(&panel_lock);
        let command = tokio::spawn(async move {
            fake2
                .get_vcp(IDENT, 0x11, &command_lock, VcpPriority::Command)
                .await
        });

        // Let command's spawn_blocking closure actually reach
        // `command_guard()` (announcing itself) before sample-2 arrives.
        tokio::time::sleep(Duration::from_millis(15)).await;

        let sample2_result = fake
            .get_vcp(IDENT, 0x12, &panel_lock, VcpPriority::Sampler)
            .await;

        let sample1_result = sample1.await.unwrap();
        let command_result = command.await.unwrap();

        assert_eq!(
            sample1_result,
            Ok(1),
            "in-flight sampler read completes normally"
        );
        assert_eq!(
            command_result,
            Ok(2),
            "command waits for sample-1, then succeeds"
        );
        assert_eq!(
            sample2_result,
            Err(VCP_SKIPPED.to_string()),
            "sample-2 must yield to the waiting command, not queue behind it"
        );

        let log = fake.take_call_log();
        let order: Vec<&str> = log
            .iter()
            .map(|l| {
                if l.contains("0x10") {
                    "sample-1"
                } else if l.contains("0x11") {
                    "command"
                } else if l.contains("0x12") {
                    "sample-2"
                } else {
                    "?"
                }
            })
            .collect();
        assert_eq!(
            order,
            vec!["sample-1", "command", "sample-2"],
            "invocation order must match the staggered spawn order: {log:?}"
        );
    }

    /// `set_vcp`'s panic path mirrors `get_vcp`'s (same `acquire` +
    /// `catch_unwind` shape) — pinned separately since it's a distinct
    /// trait method with its own scripting surface (`expect_set_panic`).
    #[tokio::test]
    async fn scripted_panic_on_set_vcp_converts_to_err() {
        let fake = single_display_fake();
        let locks = PanelLocks::new();
        let lock = locks.get(IDENT);
        fake.expect_set_panic(IDENT, 0x10, 0);

        let err = fake
            .set_vcp(IDENT, 0x10, 0, &lock, VcpPriority::Command)
            .await
            .unwrap_err();
        assert_eq!(err, VCP_PANIC);
    }
}
