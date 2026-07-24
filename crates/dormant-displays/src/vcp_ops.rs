//! Abstract DDC/CI operations (`VcpOps` trait), a real implementation backed by
//! ddc-hi (wrapped in `spawn_blocking`), and a scripted fake for unit tests.
//!
//! ## Design
//!
//! Every hardware touch is wrapped in [`tokio::task::spawn_blocking`] so the
//! async executor is never blocked. Each resolved ddc-hi `Display` handle is
//! **cached** per ident string across operations: the first call per ident
//! enumerates, finds the matching display, and stores the handle; subsequent
//! calls reuse it without re-enumeration. A cached handle is reused until one
//! of three things ends its life: a **transport/I2C error** or a **panic**
//! (caught by `catch_unwind`) drops it so the next call re-enumerates fresh —
//! covering unplug/re-plug, standby quirks, and bus renumbering; or it ages
//! past `CACHE_REVALIDATE_AFTER` (see *Revalidation window*). Protocol-level
//! errors (unsupported VCP code, checksum mismatch) do NOT invalidate — the
//! handle is healthy, only the feature is absent.
//!
//! Enumeration is ~100 ms of blocking I²C opens; re-enumerating on every poll
//! tick (3× per 2 s coordination poll) saturates the NVIDIA GPU RM driver lock
//! (~100 lock ops/s), causing compositor stutter (issue #127). Caching
//! eliminates steady-state enumeration: coordination polls run at cache-hit
//! speed, and a handle is resolved once on first use and reused for the rest
//! of its bounded life — not on every tick, and not "once per daemon start"
//! (a swapped panel on the same connector is caught by the revalidation
//! window or by a transport error mid-swap).
//!
//! ## Cache invalidation classes
//!
//! - **Transport/IO**: `"DDC/CI I2C error"`, `"MacOS kernel I/O error"`,
//!   `"Core Graphics error"`, `"Service not found"`, `"Display location
//!   not found"` — the bus or device is unreachable; cache entry dropped.
//! - **Protocol**: `"DDC/CI error:"` (unsupported VCP code, checksum
//!   mismatch, invalid length/opcode) — the display replied correctly but
//!   the feature is absent; handle is healthy, cache entry KEPT.
//! - **Panic**: `catch_unwind` converts to [`VCP_PANIC`]; cache entry dropped
//!   (the handle may be in an unknown state after a panic inside ddc-hi).
//!
//! ## Revalidation window
//!
//! Cached handles are revalidated after `CACHE_REVALIDATE_AFTER` (5 min)
//! — a panel swapped on the same connector could be driven under the old
//! ident. The deadline is **absolute**: it is stamped when a fresh
//! enumeration resolves the handle and carried unchanged through every
//! checkout/reinsert, so steady 2 s polling cannot slide it forward
//! indefinitely. Mid-window swaps are caught by error-invalidation when a
//! 2 s poll lands during the physical swap gap.
//!
//! `CACHE_REVALIDATE_AFTER` is a hard-coded const, deliberately NOT a config
//! key: it bounds an internal DDC transport-layer plumbing window, not
//! operator-facing blank/wake policy, and is behavior-neutral except for
//! limiting how long a stale identity can persist. Exposing it later would
//! not break the config schema, so the door is left open without adding a
//! knob nobody needs today.
//!
//! The trait is `#[async_trait]` so that the real implementation can `await`
//! the `spawn_blocking` join handle directly, avoiding the
//! `block_in_place`/`block_on` triple-wrap that panics on current-thread
//! runtimes.
//!
//! ## Lock discipline
//!
//! Three locks govern physical DDC access, acquired in this order and never
//! held across an `.await` (every call site is inside `spawn_blocking`):
//!
//! 1. **Panel lock** (outer, when present) — the per-panel PRIORITY mechanism
//!    via [`crate::ddc_lock::PanelLock`]: command-path callers block, sampler-
//!    path callers yield. Acquired before the DDC gate. A
//!    [`VcpPriority::Sampler`] that loses the race skips BEFORE any hardware
//!    touch — it must not acquire the gate or access the cache.
//! 2. **Physical-DDC gate** (inner) — a process-wide mutex serializing ALL
//!    physical DDC/CI traffic: every VCP transaction (cache checkout,
//!    resolution-if-miss enumeration, the VCP op, reinsert, and handle drops)
//!    AND every `list_displays` scan. A per-panel lock cannot serialize an
//!    all-bus scan against OTHER panels' locks, and a cached VCP transaction
//!    under only its panel's lock could still collide on bus Y with another
//!    panel's in-flight enumeration — the same concurrent-i²c hazard
//!    `PanelLock` exists for. The gate closes it: one physical DDC access at
//!    a time, process-wide. `list_displays` acquires the gate alone (no
//!    panel lock exists yet during `probe`). Cache teardown — the old
//!    generation's last `Arc<RealVcpState>` dropping on reload — also
//!    acquires the gate (`RealVcpState`'s `Drop`), so the cached handles'
//!    `/dev/i2c-*` closes are serialized rather than racing the new
//!    generation's gated transactions.
//! 3. **Cache mutex** (innermost) — held only for `HashMap` take/insert,
//!    never across I/O, enumeration, or panel-lock acquisition.
//!
//! VCP transactions are millisecond-scale at seconds-scale cadence, so
//! process-wide serialization is behavior-neutral: the gate is uncontended
//! in practice. Enumeration is rare (first use, invalidation, or max-age
//! revalidation); steady state is one gate acquire per VCP op against a
//! cached handle, and the operations are already spaced by the coordination
//! poll. Teardown is rarer still (one per reload) and may block briefly on a
//! runtime thread — tolerated because it is both rare and bounded.
//!
//! [`VcpPriority`] selects the panel-lock acquisition strategy:
//! [`VcpPriority::Command`] blocks until the panel is free (command/blank/
//! wake/exercise/seeding callers, which must never starve);
//! [`VcpPriority::Sampler`] tries once and yields immediately to any
//! command-path caller (periodic wear polling, which must never make a
//! command wait for it).
//!
//! A scripted or real ddc-hi panic during the physical op is caught with
//! `std::panic::catch_unwind` *inside* the guarded section (guard acquired
//! first, `catch_unwind` wraps only the op) and converted to the sentinel
//! error [`VCP_PANIC`] — a single bad transaction can never crash the
//! `spawn_blocking` worker thread or poison the panel lock's usability for
//! the next caller (`PanelLock`'s own poison recovery is defense in depth
//! for other unwind paths). A panicking transaction also invalidates the
//! cache entry for that ident.
//!
//! When [`VcpPriority::Sampler`] loses the race for the lock, the call
//! returns the sentinel error [`VCP_SKIPPED`] — never surfaced as a
//! display failure, only ever collapsed to `None` by the sampled read
//! path (mirroring how a real read error is already collapsed to `None`
//! by [`crate::ddcci::DdcciController`]'s
//! [`read_state`](dormant_core::traits::DisplayController::read_state)).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

/// Stable input-source readback error for a sampler that yielded to a
/// command-path caller. Coordination and doctor consumers match this literal
/// instead of parsing the generic VCP lock-skip error.
pub const INPUT_SOURCE_SKIPPED: &str = "skipped: command holds panel lock";

/// Sentinel error returned when the wrapped ddc-hi operation panics.
/// `catch_unwind` converts the panic into this string so a single bad
/// transaction cannot crash the `spawn_blocking` worker thread.
pub const VCP_PANIC: &str = "E_DISPLAY_IO: vcp operation panicked";

/// Information about a detected display returned by [`VcpOps::list_displays`].
///
/// `ident_string` carries the machine-local bus prefix (`backend:id …`) and
/// is the canonical panel-lock key; the EDID text fields below are the
/// bus-independent identity used by [`VcpDisplayInfo::claim_identity`] (spec F5).
#[derive(Debug, Clone)]
pub struct VcpDisplayInfo {
    /// Human-readable identifier string (backend:id manufacturer `model_name`).
    pub ident_string: String,
    /// EDID manufacturer id (e.g. `"AOC"`, `"DEL"`) — the 3-letter vendor code.
    pub manufacturer: Option<String>,
    /// EDID model/product name (e.g. `"AG326UZD"`, `"U2723QE"`).
    pub model: Option<String>,
    /// EDID serial-number string, when the descriptor is present.
    pub serial: Option<String>,
}

impl VcpDisplayInfo {
    /// Cross-machine claim identity (spec F5): `manufacturer:model`, plus
    /// `:serial` when the EDID serial is present — derived ONLY from the
    /// EDID text fields, never `ident_string` (which embeds the machine-local
    /// bus prefix). Returns `None` when manufacturer or model is absent: a
    /// panel that cannot be EDID-identified cannot be claim-matched, and
    /// must never fabricate a key. EDID text is trimmed (case-preserved) so
    /// vendor descriptor padding does not leak into the canonical key.
    #[must_use]
    pub fn claim_identity(&self) -> Option<String> {
        let manufacturer = self.manufacturer.as_deref()?.trim();
        let model = self.model.as_deref()?.trim();
        let serial = self.serial.as_deref().map(str::trim);
        if manufacturer.is_empty() || model.is_empty() {
            return None;
        }
        match serial.filter(|s| !s.is_empty()) {
            Some(serial) => Some(format!("{manufacturer}:{model}:{serial}")),
            None => Some(format!("{manufacturer}:{model}")),
        }
    }

    /// Test fixture constructor: build a [`VcpDisplayInfo`] with explicit
    /// EDID text fields so [`Self::claim_identity`] is testable without DDC
    /// hardware. `manufacturer`/`model` are stored verbatim (empty stays
    /// `Some("")` so the trim/empty-handling in [`Self::claim_identity`] is
    /// what gets exercised); `serial` is `None` when absent.
    #[cfg(test)]
    pub(crate) fn for_test(
        ident_string: &str,
        manufacturer: &str,
        model: &str,
        serial: Option<&str>,
    ) -> Self {
        Self {
            ident_string: ident_string.to_string(),
            manufacturer: Some(manufacturer.to_string()),
            model: Some(model.to_string()),
            serial: serial.map(str::to_string),
        }
    }
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

/// Maximum age for a cached handle before forced re-enumeration.
///
/// A panel swapped on the same connector could be driven under the old ident;
/// 5 min bounds the stale-identity window. Mid-window swaps are caught by
/// error-invalidation when a 2 s poll lands during the physical swap gap.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const CACHE_REVALIDATE_AFTER: Duration = Duration::from_secs(300);

/// Process-wide gate serializing ALL physical DDC/CI traffic — every VCP
/// transaction (cache checkout, resolution-if-miss enumeration, the VCP op,
/// reinsert, and handle drops) AND every `list_displays` scan. A per-panel
/// [`PanelLock`] cannot serialize an all-bus scan against OTHER panels'
/// locks, and a cached VCP transaction under only its panel's lock could
/// still collide on bus Y with another panel's in-flight enumeration — the
/// same concurrent-i²c hazard `PanelLock` exists for. The gate closes it:
/// one physical DDC access at a time, process-wide. Acquired AFTER the
/// panel lock (when present) and BEFORE the cache mutex; never held across
/// an `.await` (all call sites are inside `spawn_blocking`). See the *Lock
/// discipline* section of the module docs.
///
/// Poison recovery matches [`PanelLock`]: the guarded payload is `()`, so a
/// panicked transaction leaves nothing to distrust — acquisitions recover
/// unconditionally rather than wedging all DDC access for the life of the
/// process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
static DDC_PHYSICAL_GATE: std::sync::OnceLock<StdMutex<()>> = std::sync::OnceLock::new();

/// Acquire the process-wide physical-DDC gate. The returned guard is held
/// until the caller drops it — the entire transaction (checkout →
/// resolve-if-miss → op → reinsert → drops) runs inside. Called only inside
/// `spawn_blocking` (blocking context).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ddc_gate() -> std::sync::MutexGuard<'static, ()> {
    DDC_PHYSICAL_GATE
        .get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Returns `true` if `err_msg` indicates a transport/I2C-class error
/// (unreachable device, bus error) rather than a protocol-level reply
/// (unsupported VCP code, checksum mismatch — handle is healthy).
///
/// ddc-i2c error strings: `"DDC/CI I2C error:"` (transport) vs.
/// `"DDC/CI error:"` (protocol).
/// ddc-macos error strings: `"MacOS kernel I/O error:"`, `"Core Graphics
/// error:"`, `"Service not found"`, `"Display location not found"` are
/// transport; `"DDC/CI error:"` is protocol.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_transport_error(err_msg: &str) -> bool {
    // If it's specifically a DDC/CI protocol error (not I2C), the handle is
    // healthy — only the feature is absent.
    if err_msg.contains("DDC/CI error:") && !err_msg.contains("I2C error") {
        return false;
    }
    // Everything else is treated as transport: I2C errors, macOS kernel I/O,
    // Core Graphics failures, service-not-found, etc.
    true
}

/// Generic cache of resolved handles, parameterized over the handle type for
/// unit-testability. Production use is `HandleCache<ddc_hi::Display>`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct HandleCache<H> {
    /// Map from ident string to `(handle, resolved_at)`. `resolved_at` is the
    /// **absolute** instant the handle was freshly enumerated — stamped once
    /// at resolution and carried unchanged through every checkout/reinsert,
    /// so steady polling cannot slide the revalidation deadline forward.
    entries: StdMutex<HashMap<String, (H, Instant)>>,
    /// Resolves an ident to a `(canonical_ident, handle)` pair.
    /// Must enumerate hardware — called only on cache miss or revalidation.
    resolve_fn: fn(ident: &str) -> Result<(String, H), String>,
    /// Test seam: incremented on each call to `resolve_fn`.
    resolve_count: AtomicUsize,
    /// Entries whose age (`now − resolved_at`) is `>=` this are treated as
    /// misses and re-resolved.
    revalidate_after: Duration,
    /// Clock seam: `Instant::now` in production, injectable in tests so the
    /// absolute-deadline behavior is exercised without a 5-minute wall-clock
    /// wait.
    now_fn: fn() -> Instant,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<H> HandleCache<H> {
    fn new(
        resolve_fn: fn(ident: &str) -> Result<(String, H), String>,
        revalidate_after: Duration,
    ) -> Self {
        Self::new_with_clock(resolve_fn, revalidate_after, Instant::now)
    }

    /// Constructor with an injected clock — used by `new` (production,
    /// `Instant::now`) and by tests (a fake clock so the absolute-deadline
    /// behavior is exercised without a 5-minute wall-clock wait).
    fn new_with_clock(
        resolve_fn: fn(ident: &str) -> Result<(String, H), String>,
        revalidate_after: Duration,
        now_fn: fn() -> Instant,
    ) -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
            resolve_fn,
            resolve_count: AtomicUsize::new(0),
            revalidate_after,
            now_fn,
        }
    }

    /// Get a handle for `ident`, either from the cache or by resolving.
    /// Returns the handle **and its original `resolved_at`** — callers must
    /// pass that instant back to [`Self::return_entry`] so the deadline stays
    /// absolute. The handle is **removed** from the cache; callers must return
    /// it on success or drop it on error.
    fn get_or_resolve(&self, ident: &str) -> Result<(H, Instant), String> {
        let now = (self.now_fn)();
        // Try the cache first.
        {
            let mut entries = self.entries.lock().unwrap();
            match entries.remove(ident) {
                Some((handle, resolved_at))
                    if now.duration_since(resolved_at) < self.revalidate_after =>
                {
                    return Ok((handle, resolved_at));
                }
                // Stale entry (fd freed on drop) or no entry — fall through
                // to re-resolve below.
                _ => {}
            }
        }
        // Cache miss or expired — resolve via hardware enumeration.
        let (canonical_ident, handle) = (self.resolve_fn)(ident)?;
        self.resolve_count.fetch_add(1, Ordering::Relaxed);
        // Stamp the absolute resolution instant; reinsertion preserves it.
        let resolved_at = (self.now_fn)();
        self.entries
            .lock()
            .unwrap()
            .insert(canonical_ident, (handle, resolved_at));
        // Re-lookup — must find it.
        self.entries
            .lock()
            .unwrap()
            .remove(ident)
            .ok_or_else(|| format!("display '{ident}' not found after resolution"))
    }

    /// Return a successfully-used handle to the cache, preserving its
    /// original `resolved_at` — NOT re-stamped. This is what makes the
    /// revalidation deadline absolute rather than a sliding idle timeout.
    fn return_entry(&self, ident: &str, handle: H, resolved_at: Instant) {
        self.entries
            .lock()
            .unwrap()
            .insert(ident.to_string(), (handle, resolved_at));
    }

    /// Drain all entries for teardown. The caller (a `Drop` impl) acquires
    /// the physical-DDC gate FIRST, then calls this; the returned map drops
    /// under that gate so the cached handles' fd closes are serialized. The
    /// cache mutex is released before the map drops — only the gate spans the
    /// handle closes (gate outer, cache mutex innermost, same order as
    /// `vcp_transaction`).
    fn drain_for_teardown(&self) -> HashMap<String, (H, Instant)> {
        std::mem::take(&mut *self.entries.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Test seam: read the resolve count.
    #[cfg(test)]
    fn resolve_count(&self) -> usize {
        self.resolve_count.load(Ordering::Relaxed)
    }
}

/// Real DDC/CI operations backed by ddc-hi, with every call wrapped in
/// [`tokio::task::spawn_blocking`].
///
/// Each resolved display handle is cached per ident string via
/// `HandleCache`. Transport/I2C errors invalidate; protocol errors
/// (unsupported VCP code, etc.) do not. Handles older than
/// `CACHE_REVALIDATE_AFTER` are re-resolved.
///
/// The cache is shared across clones (the inner state is behind an
/// [`Arc`]), so cloning is cheap — every `spawn_blocking` closure moves a
/// clone to satisfy the `'static` bound.
///
/// ## Lock order (spec §4.3)
///
/// Panel lock → cache mutex (innermost). The panel lock is acquired FIRST
/// (inside `spawn_blocking`), serializing access per panel; the cache mutex
/// is held only for `HashMap` take/insert and never across I/O or
/// enumeration. Samplers that lose the panel-lock race skip BEFORE any
/// hardware touch.
///
/// Available on Linux (I²C-dev) and macOS (the vendored, path-patched
/// `ddc-macos` fork — see `vendor/ddc-macos/README.dormant.md`). On both
/// platforms `ddc_hi::Display` hides the backend behind one enum `Handle`,
/// so every method below is identical code for both — there is no
/// macOS-specific branch here.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct RealVcp {
    state: Arc<RealVcpState>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct RealVcpState {
    cache: HandleCache<ddc_hi::Display>,
}

// Clone is cheap — only the Arc reference count is bumped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Clone for RealVcp {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

/// Gate the cached handles' teardown so the `/dev/i2c-*` fd closes (NVIDIA
/// RM teardown — the #127 mechanism) are serialized like every other physical
/// DDC access. Runs when the old generation's last `Arc<RealVcpState>` drops
/// on reload — possibly on an async-runtime thread; blocking briefly here is
/// tolerated (rare, ms-scale, and the old generation is already retired from
/// service). Lock order matches `vcp_transaction`: gate (outer) → cache
/// mutex (innermost).
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for RealVcpState {
    fn drop(&mut self) {
        let _gate = ddc_gate();
        // Drain under the cache mutex; the returned map drops at the end of
        // this block, inside the gate guard, closing the cached handles.
        let _drained = self.cache.drain_for_teardown();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl RealVcp {
    /// Create a new `RealVcp` with an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(RealVcpState {
                cache: HandleCache::new(Self::resolve_display, CACHE_REVALIDATE_AFTER),
            }),
        }
    }

    /// Hardware enumeration + ident match, called by `HandleCache` on miss.
    /// Runs inside the caller's physical-DDC gate (acquired by
    /// `vcp_transaction` or `list_displays`); does NOT re-acquire it (the
    /// gate is a non-reentrant `StdMutex` — re-locking would deadlock). The
    /// non-target `Display` handles drop at the end of this fn, still inside
    /// the gate, so the `/dev/i2c-*` closes (NVIDIA RM teardown) are
    /// serialized too.
    fn resolve_display(ident: &str) -> Result<(String, ddc_hi::Display), String> {
        let displays: Vec<ddc_hi::Display> = ddc_hi::Display::enumerate();
        let mapped: Vec<(String, ddc_hi::Display)> = displays
            .into_iter()
            .map(|d| (d.info.to_string(), d))
            .collect();
        mapped
            .into_iter()
            .find(|(id, _)| id == ident)
            .ok_or_else(|| format!("display '{ident}' not found during enumeration"))
    }

    /// Test seam: read the hardware enumeration counter.
    #[cfg(test)]
    fn enumeration_count(&self) -> usize {
        self.state.cache.resolve_count()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Default for RealVcp {
    fn default() -> Self {
        Self::new()
    }
}

// ── VcpOps for RealVcp — lock order: acquire → resolve → op ───────────────

/// Run a VCP operation with cache semantics and lock discipline.
///
/// Called inside `spawn_blocking` (after the panel lock is acquired). The
/// process-wide physical-DDC gate is acquired FIRST and held through cache
/// checkout, resolution-if-miss (enumeration), the VCP op, reinsert, and
/// handle drops — so a cached transaction cannot collide on bus Y with
/// another panel's in-flight enumeration. The `op` closure receives `&mut H`
/// (the cached handle type) and must return `Result<T, String>`. On success
/// the handle is returned to the cache with its original `resolved_at`
/// preserved; on transport/IO error or panic it is dropped (invalidation);
/// on protocol error (unsupported VCP code, etc.) the handle is returned
/// transparently.
///
/// Generic over `H` so the cache + error-classification logic is testable
/// with a fake handle and a fake `resolve_fn` — no DDC hardware required
/// (see the `cache_*` tests below). Production calls it with
/// `H = ddc_hi::Display`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn vcp_transaction<H, T>(
    cache: &HandleCache<H>,
    ident: &str,
    op: impl FnOnce(&mut H) -> Result<T, String>,
) -> Result<T, String> {
    // Process-wide physical-DDC gate: held through checkout, resolution-if-
    // miss, the VCP op, reinsert, and handle drops. Acquired AFTER the panel
    // lock (the caller holds it) and BEFORE the cache mutex (innermost).
    let _gate = ddc_gate();
    let (mut handle, resolved_at) = cache.get_or_resolve(ident)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| op(&mut handle)));
    match result {
        Ok(Ok(value)) => {
            cache.return_entry(ident, handle, resolved_at);
            Ok(value)
        }
        Ok(Err(ref e)) if !is_transport_error(e) => {
            // Protocol error — handle is healthy, return it to cache.
            cache.return_entry(ident, handle, resolved_at);
            Err(e.clone())
        }
        Ok(Err(e)) => {
            // Transport error — drop the handle (invalidation).
            Err(e)
        }
        Err(_panic) => {
            // Panic — drop the handle (invalidation).
            Err(VCP_PANIC.to_string())
        }
    }
}

/// Map a ddc-hi [`DisplayInfo`] onto a [`VcpDisplayInfo`], copying the EDID
/// text fields (`manufacturer_id` / `model_name` / `serial_number`) verbatim —
/// pure, no I/O, so the EDID → claim-identity path is testable with a fixture
/// `DisplayInfo` instead of real hardware. The `ident_string` stays ddc-hi's
/// `Display::info` `to_string()` (the bus-prefixed panel-lock key, unchanged).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn vcp_display_info_from_ddc_info(info: &ddc_hi::DisplayInfo) -> VcpDisplayInfo {
    VcpDisplayInfo {
        ident_string: info.to_string(),
        manufacturer: info.manufacturer_id.clone(),
        model: info.model_name.clone(),
        serial: info.serial_number.clone(),
    }
}

/// Parsed EDID identity fields (spec F5) — the cross-machine-stable subset
/// ddc-hi's `DisplayInfo::from_edid` populates on the Linux i²c path (via
/// edid-rs) but leaves empty on macOS when the vendored `ddc-macos` fork's
/// `edid()` bytes fail edid-rs parsing.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct EdidIdentity {
    manufacturer: Option<String>,
    model: Option<String>,
    serial: Option<String>,
}

/// Parse the cross-machine EDID identity from raw EDID bytes: the manufacturer
/// PNP id (header bytes 8–9), the monitor-name descriptor (0xFC), and the
/// serial-number descriptor (0xFF).
///
/// Pure, no I/O, no macOS API — so the EDID → claim-identity path is testable
/// on Linux with a fixture blob. Mirrors what ddc-hi's `DisplayInfo::from_edid`
/// (via edid-rs) populates on the Linux i²c path: `manufacturer_id` from the
/// header vendor bytes, `model_name` from descriptor 0xFC, `serial_number`
/// from descriptor 0xFF. The numeric EDID serial field (bytes 12–15) is
/// deliberately NOT used: the desktop derives `serial_number` from the 0xFF
/// string descriptor, so a numeric fallback would diverge and break the
/// byte-identical cross-machine match (spec F5).
///
/// Returns `None` only for a structurally too-short EDID (< 128 bytes); a
/// present-but-empty descriptor yields `EdidIdentity` with `None` fields,
/// which [`VcpDisplayInfo::claim_identity`] collapses to `None` (honest).
#[cfg(any(target_os = "macos", test))]
fn parse_edid_identity(edid: &[u8]) -> Option<EdidIdentity> {
    // The base block is 128 bytes; the four 18-byte descriptor slots live at
    // offsets 54..126. Extension blocks do not carry base identity.
    if edid.len() < 128 {
        return None;
    }
    let manufacturer = decode_pnp_manufacturer(edid[8], edid[9]);
    let (model, serial) = parse_monitor_descriptors(&edid[54..126]);
    Some(EdidIdentity {
        manufacturer,
        model,
        serial,
    })
}

/// Decode the 3-letter EDID manufacturer PNP id from the two header bytes
/// (big-endian u16, three 5-bit groups, 1–26 → 'A'–'Z'). A 0 group means
/// "unused" and is skipped rather than fabricated into a partial id.
#[cfg(any(target_os = "macos", test))]
fn decode_pnp_manufacturer(high: u8, low: u8) -> Option<String> {
    let id = u16::from_be_bytes([high, low]);
    let groups = [(id >> 10) & 0x1F, (id >> 5) & 0x1F, id & 0x1F];
    let mut s = String::new();
    for g in groups {
        if let 1..=26 = g {
            s.push(
                char::from_u32(u32::from(g) - 1 + u32::from(b'A'))
                    .expect("1..=26 maps to 'A'..='Z'"),
            );
        }
    }
    if s.is_empty() { None } else { Some(s) }
}

/// Walk the four 18-byte descriptor slots and pull the first monitor-name
/// (0xFC) and serial-number (0xFF) string descriptors. A slot is a monitor
/// descriptor (vs a detailed timing) when it starts with `0x00 0x00 0x00`;
/// per EDID 1.4 §3.10.4 byte 3 is then the tag and bytes 5..18 the data
/// (byte 4 is reserved zero). The AOC's panel on this Mac puts the tag at
/// byte 3 — the previous code checked byte 2, which is always zero, and
/// so never matched any descriptor.
#[cfg(any(target_os = "macos", test))]
fn parse_monitor_descriptors(slots: &[u8]) -> (Option<String>, Option<String>) {
    let mut model = None;
    let mut serial = None;
    for block in slots.chunks_exact(18) {
        // 0x00 0x00 0x00 = monitor-descriptor signature (per EDID 1.4 §3.10.4).
        if block[0] != 0 || block[1] != 0 || block[2] != 0 {
            continue;
        }
        let text = descriptor_string(&block[5..18]);
        match (block[3], text) {
            (0xFC, Some(t)) if model.is_none() => model = Some(t),
            (0xFF, Some(t)) if serial.is_none() => serial = Some(t),
            _ => {}
        }
    }
    (model, serial)
}

/// Extract a null/LF-terminated, space-padded ASCII string from a descriptor
/// data range. Trailing whitespace is trimmed; a non-UTF8 or empty result is
/// `None`.
#[cfg(any(target_os = "macos", test))]
fn descriptor_string(data: &[u8]) -> Option<String> {
    let end = data
        .iter()
        .position(|b| *b == 0x00 || *b == 0x0A)
        .unwrap_or(data.len());
    let s = std::str::from_utf8(&data[..end]).ok()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Build a [`VcpDisplayInfo`] from a ddc-hi [`ddc_hi::Display`], deriving the
/// EDID identity from ddc-hi's parsed `DisplayInfo` and — on macOS only —
/// backfilling it from the vendored `ddc-macos` fork's raw EDID bytes when
/// ddc-hi's `from_edid` left it empty (the F5 gap: `claim_identity` derived
/// `None` on macOS). The `ident_string` is always ddc-hi's `Display::info`
/// `to_string()`, so the panel-lock key and cache resolution are unchanged.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn vcp_display_info_from_display(d: &ddc_hi::Display) -> VcpDisplayInfo {
    // `mut` only for the macOS backfill below; on Linux the backfill is
    // cfg'd out, so suppress the otherwise-unused `mut` there.
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut vcp = vcp_display_info_from_ddc_info(&d.info);
    #[cfg(target_os = "macos")]
    backfill_edid_identity_from_macos(&mut vcp, &d.handle);
    vcp
}

/// macOS-only backfill: derive the EDID identity fields from the vendored
/// `ddc-macos` fork's raw EDID bytes when ddc-hi's `DisplayInfo::from_edid`
/// left them empty. ddc-hi reaches the fork's `Monitor` through its
/// `Handle::MacOS` variant (the variant is `#[doc(hidden)]` but public; ddc-hi
/// is pinned at 0.4). The fork's `edid()` returns the same panel-intrinsic
/// bytes the Linux i²c path parses, so the shared panel's `claim_identity` is
/// byte-identical across machines (spec F5).
#[cfg(target_os = "macos")]
fn backfill_edid_identity_from_macos(vcp: &mut VcpDisplayInfo, handle: &ddc_hi::Handle) {
    if vcp.claim_identity().is_some() {
        return;
    }
    let ddc_hi::Handle::MacOS(monitor) = handle;
    let Some(edid) = monitor.edid() else {
        return;
    };
    let Some(id) = parse_edid_identity(&edid) else {
        return;
    };
    vcp.manufacturer = id.manufacturer;
    vcp.model = id.model;
    vcp.serial = id.serial;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[async_trait]
impl VcpOps for RealVcp {
    async fn list_displays(&self) -> Vec<VcpDisplayInfo> {
        // Acquire the physical-DDC gate for the whole scan — enumerate,
        // conversion, and the non-target handle drops (NVIDIA RM teardown)
        // all run inside it. No panel lock exists yet (probe derives identity
        // from this scan), so the gate is acquired alone.
        tokio::task::spawn_blocking(|| {
            let _gate = ddc_gate();
            ddc_hi::Display::enumerate()
                .into_iter()
                .map(|d| vcp_display_info_from_display(&d))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default()
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
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            // Panel lock acquired before cache resolution and held outside
            // `catch_unwind` (inside `vcp_transaction`) — a panic in the
            // wrapped op therefore never poisons the mutex.
            let _guard = acquire(&lock, prio)?;
            vcp_transaction(&me.state.cache, &ident, |display| {
                let vcp = display
                    .handle
                    .get_vcp_feature(code)
                    .map_err(|e| format!("get_vcp(0x{code:02X}) failed: {e}"))?;
                Ok(vcp.value())
            })
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
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = acquire(&lock, prio)?;
            vcp_transaction(&me.state.cache, &ident, |display| {
                display
                    .handle
                    .set_vcp_feature(code, value)
                    .map_err(|e| format!("set_vcp(0x{code:02X}, {value}) failed: {e}"))
            })
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
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = acquire(&lock, prio)?;
            vcp_transaction(&me.state.cache, &ident, |display| {
                let vcp = display
                    .handle
                    .get_vcp_feature(code)
                    .map_err(|e| format!("get_vcp_raw(0x{code:02X}) failed: {e}"))?;
                Ok([vcp.mh, vcp.ml, vcp.sh, vcp.sl])
            })
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
    /// Set-VCP calls with their lock-acquisition priority. This is separate
    /// from the string log so tests can assert command/sampler behavior
    /// without coupling to its diagnostic formatting.
    set_calls: StdMutex<Vec<(u8, u16, VcpPriority)>>,
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
            set_calls: StdMutex::new(Vec::new()),
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

    /// Snapshot Set-VCP calls and their requested priority.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn set_calls(&self) -> Vec<(u8, u16, VcpPriority)> {
        self.set_calls.lock().unwrap().clone()
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
        self.set_calls.lock().unwrap().push((code, value, prio));
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
            manufacturer: None,
            model: None,
            serial: None,
        }])
    }

    const IDENT: &str = "i2c-dev:1 TST TEST";

    // ── F5: claim_identity (EDID-derived, bus-independent) ─────────────────

    /// F5: two display infos that differ ONLY in the machine-local bus
    /// prefix (`i2c:7` vs `iokit:4`) derive the SAME claim identity — the
    /// broadcast claim model's hard precondition (byte-identical across
    /// machines). `ident_string` carries the bus prefix; `claim_identity`
    /// is derived only from EDID manufacturer/model/serial.
    #[test]
    fn claim_identity_is_machine_local_bus_independent() {
        let a = VcpDisplayInfo::for_test("i2c:7 AOC AG326UZD", "AOC", "AG326UZD", Some("ABC123"));
        let b = VcpDisplayInfo::for_test("iokit:4 AOC AG326UZD", "AOC", "AG326UZD", Some("ABC123"));
        assert_eq!(a.claim_identity(), Some("AOC:AG326UZD:ABC123".into()));
        assert_eq!(a.claim_identity(), b.claim_identity());
    }

    /// EDID text is trimmed (case-preserved) before formatting — vendors
    /// pad descriptor strings with whitespace, which would otherwise leak
    /// into the canonical key and break byte-identical matching.
    #[test]
    fn claim_identity_trims_edid_fields() {
        let a = VcpDisplayInfo::for_test(
            "i2c:7 AOC AG326UZD",
            "  AOC  ",
            " AG326UZD ",
            Some("  ABC123  "),
        );
        assert_eq!(a.claim_identity(), Some("AOC:AG326UZD:ABC123".into()));
    }

    /// An absent EDID serial degrades to `manufacturer:model` (no trailing
    /// colon) — still specific enough to claim-match, and honest about what
    /// the panel exposed.
    #[test]
    fn claim_identity_absent_serial_produces_manufacturer_model() {
        let a = VcpDisplayInfo::for_test("i2c:7 AOC AG326UZD", "AOC", "AG326UZD", None);
        assert_eq!(a.claim_identity(), Some("AOC:AG326UZD".into()));
    }

    /// Missing manufacturer → `None` (honest default): a panel that did not
    /// expose an EDID manufacturer cannot be claim-matched, and must never
    /// fabricate a key from model+serial alone.
    #[test]
    fn claim_identity_missing_manufacturer_returns_none() {
        let a = VcpDisplayInfo::for_test("i2c:7 AOC AG326UZD", "", "AG326UZD", Some("ABC123"));
        assert_eq!(a.claim_identity(), None);
    }

    /// Missing model → `None`, symmetric with the manufacturer case.
    #[test]
    fn claim_identity_missing_model_returns_none() {
        let a = VcpDisplayInfo::for_test("i2c:7 AOC AG326UZD", "AOC", "", Some("ABC123"));
        assert_eq!(a.claim_identity(), None);
    }

    /// The production path: ddc-hi leaves `manufacturer_id` `None` when the
    /// EDID lacks a manufacturer descriptor. `claim_identity` must treat that
    /// the same as a missing field (not as `Some("")`).
    #[test]
    fn claim_identity_none_when_manufacturer_field_absent() {
        let info = VcpDisplayInfo {
            ident_string: "i2c:7 AOC AG326UZD".into(),
            manufacturer: None,
            model: Some("AG326UZD".into()),
            serial: Some("ABC123".into()),
        };
        assert_eq!(info.claim_identity(), None);
    }

    /// `vcp_display_info_from_ddc_info` maps the EDID text fields a Linux
    /// i2c-dev backend exposes onto [`VcpDisplayInfo`], so `claim_identity`
    /// is derivable from a real enumeration without parsing `ident_string`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn vcp_display_info_from_ddc_info_maps_edid_fields_i2c() {
        let mut info = ddc_hi::DisplayInfo::new(ddc_hi::Backend::I2cDevice, "7".into());
        info.manufacturer_id = Some("AOC".into());
        info.model_name = Some("AG326UZD".into());
        info.serial_number = Some("ABC123".into());
        let vcp = vcp_display_info_from_ddc_info(&info);
        assert_eq!(vcp.manufacturer.as_deref(), Some("AOC"));
        assert_eq!(vcp.model.as_deref(), Some("AG326UZD"));
        assert_eq!(vcp.serial.as_deref(), Some("ABC123"));
        assert_eq!(vcp.claim_identity(), Some("AOC:AG326UZD:ABC123".into()));
    }

    /// macOS `IOKit` backend: the same EDID fields produce the SAME
    /// `claim_identity` as the Linux i2c-dev case — the bus-independence
    /// property at the mapper level (F5 pre-implementation gate, unit form).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn vcp_display_info_from_ddc_info_maps_edid_fields_macos() {
        let mut info = ddc_hi::DisplayInfo::new(ddc_hi::Backend::MacOS, "4".into());
        info.manufacturer_id = Some("AOC".into());
        info.model_name = Some("AG326UZD".into());
        info.serial_number = Some("ABC123".into());
        let vcp = vcp_display_info_from_ddc_info(&info);
        assert_eq!(
            vcp.claim_identity(),
            Some("AOC:AG326UZD:ABC123".into()),
            "macOS IOKit and Linux i2c must derive the same claim identity"
        );
    }

    // ── F5 fix: EDID-byte extraction (macOS fork path), Leg 2 fixture tests ──

    /// Recompute the EDID base-block checksum (byte 127) so a mutated fixture
    /// stays descriptor-correct.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn recompute_checksum(e: &mut [u8]) {
        let sum: u32 = e[..127].iter().copied().map(u32::from).sum();
        e[127] = u8::try_from((256 - (sum % 256)) % 256).expect("checksum is 0..=255");
    }

    /// Build a 128-byte base EDID block for the AOC AG326UZD (serial
    /// XK2R9JA000013) with a valid checksum — a descriptor-correct fixture for
    /// `parse_edid_identity` (T1 fix, Leg 2).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn ag326uzd_edid() -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        // manufacturer AOC = (1<<10)|(15<<5)|3 = 0x05E3 (big-endian)
        e[8..10].copy_from_slice(&[0x05, 0xE3]);
        e[18] = 0x01; // EDID version
        e[19] = 0x04; // revision
        // descriptor 1 (offset 54): monitor name 0xFC = "AG326UZD".
        // EDID 1.4 §3.10.4 monitor-descriptor layout: bytes 0-1 = 0x00 0x00
        // signature, byte 2 reserved 0x00, byte 3 tag, byte 4 reserved 0x00,
        // bytes 5..18 data. The AOC on this Mac uses the standard layout —
        // the previous non-standard fixture (tag at byte 2) masked the bug.
        e[54] = 0x00;
        e[55] = 0x00;
        e[56] = 0x00;
        e[57] = 0xFC;
        e[58] = 0x00;
        let name = b"AG326UZD";
        e[59..59 + name.len()].copy_from_slice(name);
        e[59 + name.len()] = 0x0A; // LF terminator
        // descriptor 2 (offset 72): serial 0xFF = "XK2R9JA000013"
        e[72] = 0x00;
        e[73] = 0x00;
        e[74] = 0x00;
        e[75] = 0xFF;
        e[76] = 0x00;
        let serial = b"XK2R9JA000013";
        e[77..77 + serial.len()].copy_from_slice(serial);
        e[77 + serial.len()] = 0x0A;
        recompute_checksum(&mut e);
        e
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_edid_identity_extracts_aoc_ag326uzd_serial() {
        let id = parse_edid_identity(&ag326uzd_edid()).expect("128-byte EDID parses");
        assert_eq!(id.manufacturer.as_deref(), Some("AOC"));
        assert_eq!(id.model.as_deref(), Some("AG326UZD"));
        assert_eq!(id.serial.as_deref(), Some("XK2R9JA000013"));
    }

    /// The whole point of F5: the macOS EDID-derived identity must be
    /// byte-identical to the Linux i²c path's, so the claim broadcast matches
    /// the same physical panel across machines.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_edid_identity_matches_linux_claim_identity_byte_for_byte() {
        let id = parse_edid_identity(&ag326uzd_edid()).expect("128-byte EDID parses");
        let vcp = VcpDisplayInfo {
            ident_string: "macos:4 AOC AG326UZD".into(),
            manufacturer: id.manufacturer,
            model: id.model,
            serial: id.serial,
        };
        assert_eq!(
            vcp.claim_identity().as_deref(),
            Some("AOC:AG326UZD:XK2R9JA000013"),
            "macOS EDID-derived identity must be byte-identical to the Linux path"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_edid_identity_absent_serial_descriptor_drops_serial() {
        let mut e = ag326uzd_edid();
        e[72..90].fill(0); // clear the 0xFF descriptor
        recompute_checksum(&mut e);
        let id = parse_edid_identity(&e).expect("128-byte EDID parses");
        assert_eq!(id.manufacturer.as_deref(), Some("AOC"));
        assert_eq!(id.model.as_deref(), Some("AG326UZD"));
        assert!(id.serial.is_none());
        let vcp = VcpDisplayInfo {
            ident_string: "x".into(),
            manufacturer: id.manufacturer,
            model: id.model,
            serial: id.serial,
        };
        assert_eq!(vcp.claim_identity().as_deref(), Some("AOC:AG326UZD"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_edid_identity_absent_model_descriptor_yields_none_claim() {
        let mut e = ag326uzd_edid();
        e[54..72].fill(0); // clear the 0xFC descriptor
        recompute_checksum(&mut e);
        let id = parse_edid_identity(&e).expect("128-byte EDID parses");
        assert!(id.model.is_none());
        let vcp = VcpDisplayInfo {
            ident_string: "x".into(),
            manufacturer: id.manufacturer,
            model: id.model,
            serial: id.serial,
        };
        assert_eq!(
            vcp.claim_identity(),
            None,
            "no model → claim_identity None (honest)"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_edid_identity_too_short_returns_none() {
        assert!(parse_edid_identity(&[0u8; 64]).is_none());
    }

    /// Captured from a real Mac M3 Pro's `IORegistry` (`AppleDisplayCrossbar`
    /// → `AppleATCDPINAdapterPort` → `IOPortTransportStateDisplayPort` →
    /// `Metadata.EDID`) on the AOC AG326UZD attached via USB-C DP Alt-Mode.
    /// 384 bytes = 128-byte base block + 256-byte CTA extension; the base
    /// block alone is sufficient for `parse_edid_identity`. This is the
    /// byte-exact fixture that exposed the v1 bug — the fork lookup path
    /// surfaced a 128+256-byte EDID where the monitor descriptors put the
    /// tag at byte 3 (EDID 1.4 §3.10.4), not byte 2 as v1's parser
    /// assumed.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_real_mac_arm_edid_byte_identical_to_desktop() {
        let edid = include_bytes!("fixtures/aoc-ag326uzd-mac.edid");
        let id = parse_edid_identity(edid).expect("Mac EDID parses");
        assert_eq!(id.manufacturer.as_deref(), Some("AOC"));
        assert_eq!(id.model.as_deref(), Some("AG326UZD"));
        assert_eq!(id.serial.as_deref(), Some("XK2R9JA000013"));
        let vcp = VcpDisplayInfo {
            ident_string: "macos:4 AOC AG326UZD".into(),
            manufacturer: id.manufacturer,
            model: id.model,
            serial: id.serial,
        };
        assert_eq!(
            vcp.claim_identity().as_deref(),
            Some("AOC:AG326UZD:XK2R9JA000013"),
            "macOS EDID-derived identity must be byte-identical to the Linux i²c path"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn decode_pnp_manufacturer_aoc_and_del() {
        assert_eq!(decode_pnp_manufacturer(0x05, 0xE3).as_deref(), Some("AOC"));
        // DEL = (4<<10)|(5<<5)|12 = 0x10AC
        assert_eq!(decode_pnp_manufacturer(0x10, 0xAC).as_deref(), Some("DEL"));
    }

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

    // ── HandleCache hardware-free tests ───────────────────────────────────
    //
    // The cache + error-classification logic is exercised through the generic
    // `HandleCache<H>` + `vcp_transaction` seam with a fake resolve function
    // and a fake handle — no DDC hardware required, every assertion
    // unconditional (no skip-to-green). The three `RealVcp` hardware tests
    // below are `#[ignore]`d: they passed vacuously on headless machines and
    // assume an unsupported VCP code invalidates the cache (it does not —
    // only transport errors do).

    /// Fake resolved handle for the hardware-free cache tests.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[derive(Debug, Clone, PartialEq)]
    struct TestHandle(u32);

    /// Fake `resolve_fn` for the hardware-free cache tests — returns a fresh
    /// handle for any ident. Call count is tracked by `HandleCache::resolve_count`.
    ///
    /// The `Result` wrapping is required by the `HandleCache::resolve_fn`
    /// signature contract (`fn(&str) -> Result<(String, H), String>`), not by
    /// this helper's logic — it never fails.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[allow(clippy::unnecessary_wraps)]
    fn test_resolve(ident: &str) -> Result<(String, TestHandle), String> {
        Ok((ident.to_string(), TestHandle(0)))
    }

    /// Two sequential ops against the same ident cause exactly ONE resolve —
    /// the second op hits the cache.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cache_sequential_hit_one_resolve_per_two_ops() {
        let cache: HandleCache<TestHandle> = HandleCache::new(test_resolve, CACHE_REVALIDATE_AFTER);
        let ident = "i2c-dev:1 TST TEST";

        vcp_transaction(&cache, ident, |_| Ok::<u32, String>(1)).unwrap();
        assert_eq!(cache.resolve_count(), 1, "first op resolves");

        vcp_transaction(&cache, ident, |_| Ok::<u32, String>(2)).unwrap();
        assert_eq!(
            cache.resolve_count(),
            1,
            "second op must hit the cache — no re-resolve"
        );
    }

    /// A transport/I2C error invalidates the cache entry — the next op
    /// re-resolves. The error-bearing op itself hits the cache (it does not
    /// re-resolve); invalidation happens after the op, by dropping the handle.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cache_transport_error_invalidates() {
        let cache: HandleCache<TestHandle> = HandleCache::new(test_resolve, CACHE_REVALIDATE_AFTER);
        let ident = "i2c-dev:1 TST TEST";

        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(cache.resolve_count(), 1);

        let err = vcp_transaction(&cache, ident, |_| {
            Err::<(), String>("get_vcp(0x10) failed: DDC/CI I2C error: read failed".into())
        })
        .unwrap_err();
        assert!(err.contains("I2C error"), "transport error surfaces: {err}");
        assert_eq!(
            cache.resolve_count(),
            1,
            "the transport-error op hit the cache — it does not re-resolve"
        );

        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(
            cache.resolve_count(),
            2,
            "transport error must invalidate → next op re-resolves"
        );
    }

    /// A protocol error (unsupported VCP code, checksum mismatch) does NOT
    /// invalidate — the handle is healthy, only the feature is absent. The
    /// next op hits the cache.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cache_protocol_error_does_not_invalidate() {
        let cache: HandleCache<TestHandle> = HandleCache::new(test_resolve, CACHE_REVALIDATE_AFTER);
        let ident = "i2c-dev:1 TST TEST";

        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(cache.resolve_count(), 1);

        let err = vcp_transaction(&cache, ident, |_| {
            Err::<(), String>("get_vcp(0xD6) failed: DDC/CI error: unsupported VCP code".into())
        })
        .unwrap_err();
        assert!(
            err.contains("DDC/CI error:"),
            "protocol error surfaces: {err}"
        );
        assert!(!err.contains("I2C error"));

        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(
            cache.resolve_count(),
            1,
            "protocol error must NOT invalidate → next op hits the cache"
        );
    }

    /// A panic inside the op invalidates the cache entry — the next op
    /// re-resolves.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cache_panic_invalidates() {
        let cache: HandleCache<TestHandle> = HandleCache::new(test_resolve, CACHE_REVALIDATE_AFTER);
        let ident = "i2c-dev:1 TST TEST";

        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(cache.resolve_count(), 1);

        let err = vcp_transaction(&cache, ident, |_| -> Result<(), String> {
            panic!("ddc-hi boom")
        })
        .unwrap_err();
        assert_eq!(err, VCP_PANIC, "panic converts to the VCP_PANIC sentinel");

        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(
            cache.resolve_count(),
            2,
            "panic must invalidate → next op re-resolves"
        );
    }

    /// The revalidation deadline is ABSOLUTE, not a sliding idle timeout:
    /// `resolved_at` is stamped once at resolution and carried unchanged
    /// through every checkout/reinsert, so steady polling well inside the
    /// window cannot push the deadline forward forever. A fake clock advances
    /// "2 s" per hit (the coordination-poll cadence) against a 5-min window;
    /// `resolve_count` must stay 1 until total elapsed crosses the absolute
    /// deadline, then go to 2 — never before. This fails against a reinsert
    /// that re-stamps `now()` (the deadline would slide and never fire).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cache_max_age_absolute_deadline_survives_steady_polling() {
        // Fake clock: a fixed base (captured once) plus a test-advanced offset.
        // The base cancels out of every `now − resolved_at` age computation, so
        // only the offset delta matters — fully deterministic, no wall-clock
        // wait and no `tokio::time::sleep` (which the test-timing gate rejects).
        static OFFSET_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        fn fake_now() -> Instant {
            *BASE.get_or_init(Instant::now)
                + Duration::from_millis(OFFSET_MS.load(std::sync::atomic::Ordering::Relaxed))
        }
        fn advance(ms: u64) {
            OFFSET_MS.fetch_add(ms, std::sync::atomic::Ordering::Relaxed);
        }

        OFFSET_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        let cache = HandleCache::<TestHandle>::new_with_clock(
            test_resolve,
            CACHE_REVALIDATE_AFTER,
            fake_now,
        );
        let ident = "i2c-dev:1 TST TEST";

        // First op resolves at t=0.
        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(cache.resolve_count(), 1);

        // Poll every 2 s — each hit is well inside the 5-min (300 s) window, so
        // each is a cache hit. The absolute age grows toward the deadline.
        for hit in 1..=149 {
            advance(2_000);
            vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
            assert_eq!(
                cache.resolve_count(),
                1,
                "hit {hit} at ~{} s is inside the 300 s window — must hit, not re-resolve",
                hit * 2,
            );
        }

        // 149 hits × 2 s = 298 s < 300 s: still cached. One more 2 s crosses the
        // absolute deadline → re-resolve.
        advance(2_000);
        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(
            cache.resolve_count(),
            2,
            "after the 300 s absolute deadline, the entry must re-resolve — \
             a sliding (re-stamped) deadline would still be 2 s old here and \
             would NOT have re-resolved"
        );
    }

    /// A sampler that loses the panel-lock race skips BEFORE any enumeration
    /// or cache touch — `enumeration_count` stays zero. Hardware-free: the
    /// skip happens before `resolve_display` is ever called, so no DDC device
    /// is needed.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn sampler_skip_before_enumeration() {
        let vcp = RealVcp::new();
        let locks = PanelLocks::new();
        let lock = locks.get(IDENT);
        // Hold the panel lock as a command-path caller would.
        let held = lock.command_guard();

        let err = vcp
            .get_vcp(IDENT, 0x10, &lock, VcpPriority::Sampler)
            .await
            .unwrap_err();
        assert_eq!(err, VCP_SKIPPED, "sampler must skip via VCP_SKIPPED");
        assert_eq!(
            vcp.enumeration_count(),
            0,
            "sampler skip must happen BEFORE any enumeration or cache touch"
        );
        drop(held);
    }

    /// A cached VCP op (cache hit, no resolve) still acquires the process-wide
    /// physical-DDC gate — the gate serializes ALL physical DDC traffic, not
    /// just enumeration. Proven deterministically: the op closure runs inside
    /// `vcp_transaction`'s gate, so a `try_lock` on the gate from within the
    /// closure must return `WouldBlock`. No timing, no threads — if the gate
    /// were NOT acquired, `try_lock` would succeed and the assertion would
    /// fail. Uses the HandleCache/fake-handle seam (no DDC hardware).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cached_op_acquires_physical_gate() {
        let cache = HandleCache::<TestHandle>::new(test_resolve, CACHE_REVALIDATE_AFTER);
        let ident = "i2c-dev:1 TST TEST";
        // Populate the cache (acquires + releases the gate).
        vcp_transaction(&cache, ident, |_| Ok::<(), String>(())).unwrap();
        assert_eq!(cache.resolve_count(), 1);

        // Cached op (cache hit). The closure observes the gate state from
        // inside vcp_transaction's critical section — try_lock MUST find the
        // gate held (WouldBlock). Deterministic: no scheduler dependence.
        vcp_transaction(&cache, ident, |_| {
            let gate = DDC_PHYSICAL_GATE
                .get()
                .expect("DDC_PHYSICAL_GATE initialized by the populate call above");
            assert!(
                matches!(gate.try_lock(), Err(std::sync::TryLockError::WouldBlock)),
                "vcp_transaction must hold the physical-DDC gate during the op closure"
            );
            Ok::<(), String>(())
        })
        .unwrap();
        // Still a cache hit — no re-resolve.
        assert_eq!(
            cache.resolve_count(),
            1,
            "cached op must hit the cache — no re-resolve"
        );
    }

    // ── RealVcp cache tests (require DDC-capable display) ────────────────────

    /// Test seam: read the hardware-enumeration counter inside `RealVcp`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn real_vcp_enum_count(vcp: &RealVcp) -> usize {
        vcp.enumeration_count()
    }

    /// Enumerate once to discover the ident of the first DDC display.
    /// Returns `None` if no display is available (CI / headless). Runs under
    /// the physical-DDC gate like every other enumerate call site.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn first_display_ident() -> Option<String> {
        let _gate = ddc_gate();
        ddc_hi::Display::enumerate()
            .first()
            .map(|d| d.info.to_string())
    }

    /// (a) Two sequential `get_vcp` calls against the same display cause
    /// exactly ONE hardware enumeration — the second call hits the cache.
    ///
    /// RED baseline (before the cache): 2 enumerations for 2 calls.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[ignore = "superseded by the hardware-free cache_sequential_hit_one_resolve_per_two_ops; run on hardware via --ignored"]
    #[tokio::test]
    async fn cache_hit_avoids_re_enumeration() {
        let Some(ident) = first_display_ident() else {
            eprintln!("SKIP: no DDC displays available");
            return;
        };

        let vcp = RealVcp::new();
        let locks = PanelLocks::new();
        let lock = locks.get(&ident);

        // First call — enumerates hardware, populates cache.
        let r1 = vcp.get_vcp(&ident, 0x10, &lock, VcpPriority::Command).await;
        let count_after_first = real_vcp_enum_count(&vcp);

        // Second call — must hit the cache, no re-enumeration.
        let r2 = vcp.get_vcp(&ident, 0x10, &lock, VcpPriority::Command).await;
        let count_after_second = real_vcp_enum_count(&vcp);

        // If the first call failed (unsupported VCP code, busy monitor, …),
        // nothing was cached — the count assertion won't hold.  The RED
        // property this test exists to catch is "every call enumerates",
        // which is what the second assertion proves has stopped.
        if r1.is_ok() && r2.is_ok() {
            assert!(
                count_after_first >= 1,
                "first call must enumerate at least once (got {count_after_first})"
            );
            assert_eq!(
                count_after_second, count_after_first,
                "second call must NOT re-enumerate (cache hit); \
                 first count={count_after_first}, second={count_after_second}"
            );
        } else {
            eprintln!(
                "SKIP: VCP op failed (first={r1:?}, second={r2:?}) — \
                 cache not populated, count assertion inapplicable"
            );
        }
    }

    /// (b) An operation error drops the cache entry, so the *next* call
    /// after the error re-enumerates (counter increments).
    ///
    /// Strategy: populate the cache via a successful `get_vcp(0x10)`, then
    /// issue a `set_vcp` with a likely-unsupported code (`0xDF`) to trigger
    /// a ddc-hi error.  The error must invalidate the cache, so the
    /// subsequent `get_vcp(0x10)` enumerates fresh.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[ignore = "superseded by the hardware-free cache_transport_error_invalidates / cache_protocol_error_does_not_invalidate; run on hardware via --ignored"]
    #[tokio::test]
    async fn error_invalidates_cache_entry() {
        let Some(ident) = first_display_ident() else {
            eprintln!("SKIP: no DDC displays available");
            return;
        };

        let vcp = RealVcp::new();
        let locks = PanelLocks::new();
        let lock = locks.get(&ident);

        // Populate cache.
        let r1 = vcp.get_vcp(&ident, 0x10, &lock, VcpPriority::Command).await;
        let count_after_first = real_vcp_enum_count(&vcp);

        // Second call: use an unsupported VCP code to trigger an error.
        // 0xDF (VCP feature 223) is not in any MCCS spec category — most
        // monitors reject it.
        let r2 = vcp
            .set_vcp(&ident, 0xDF, 0, &lock, VcpPriority::Command)
            .await;

        // Third call: must re-enumerate (cache was invalidated by the error).
        let _r3 = vcp.get_vcp(&ident, 0x10, &lock, VcpPriority::Command).await;
        let count_after_third = real_vcp_enum_count(&vcp);

        if r1.is_ok() && r2.is_err() {
            assert!(
                count_after_first >= 1,
                "first call must enumerate at least once (got {count_after_first})"
            );
            assert!(
                count_after_third > count_after_first,
                "error on second call must invalidate cache → third call re-enumerates; \
                 first={count_after_first}, third={count_after_third}"
            );
        } else {
            eprintln!(
                "SKIP: preconditions not met (first={r1:?}, second={r2:?}) — \
                 first must succeed and second must error to exercise invalidation"
            );
        }
    }

    /// (c) A panicking VCP transaction also invalidates the cache entry.
    ///
    /// This test exercises the `unwrap_or_else` path in `get_vcp` /
    /// `set_vcp` / `get_vcp_raw`: when `catch_unwind` returns `Err`
    /// (i.e. the wrapped op panicked), the match arm `_ => {}` must drop
    /// the display handle rather than returning it to the cache.
    ///
    /// Without a mock seam to inject a panic into a `ddc_hi` call, this
    /// test verifies the invalidation structure by directly exercising the
    /// `RealVcp` cache primitives: it populates the cache, then simulates
    /// an error (via an unsupported VCP code), and asserts the
    /// re-enumeration on the next call — the same invalidation branch
    /// (`_ => {}`) that catches panics also catches errors, so coverage
    /// of the error branch implies coverage of the panic branch.
    ///
    /// On hardware where the unsupported-code write succeeds (rare), the
    /// test vacuously passes (nothing to invalidate).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[ignore = "superseded by the hardware-free cache_panic_invalidates; run on hardware via --ignored"]
    #[tokio::test]
    async fn panic_path_invalidates_cache() {
        let Some(ident) = first_display_ident() else {
            eprintln!("SKIP: no DDC displays available");
            return;
        };

        let vcp = RealVcp::new();
        let locks = PanelLocks::new();
        let lock = locks.get(&ident);

        // Populate cache via a successful get_vcp.
        let r1 = vcp.get_vcp(&ident, 0x10, &lock, VcpPriority::Command).await;
        let count_after_first = real_vcp_enum_count(&vcp);

        // Issue a set_vcp with an unsupported code to hit the error
        // invalidation path (same `_ => {}` branch as a panic).
        let r2 = vcp
            .set_vcp(&ident, 0xDF, 0, &lock, VcpPriority::Command)
            .await;

        // Verify cache was invalidated: next call re-enumerates.
        let _r3 = vcp.get_vcp(&ident, 0x10, &lock, VcpPriority::Command).await;
        let count_after_third = real_vcp_enum_count(&vcp);

        if r1.is_ok() && r2.is_err() {
            assert!(
                count_after_third > count_after_first,
                "invalidation (error/panic branch) must cause re-enumeration; \
                 first={count_after_first}, third={count_after_third}"
            );
        } else {
            eprintln!("SKIP: preconditions not met (first={r1:?}, second={r2:?})");
        }
    }
}
