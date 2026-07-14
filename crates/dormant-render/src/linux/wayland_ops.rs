//! `WaylandOps` — a narrow, object-safe seam around the `wp_viewport`
//! protocol requests `dormant-render`'s Linux backend issues at runtime,
//! following the `VcpOps` pattern in `dormant-displays`
//! (`crates/dormant-displays/src/vcp_ops.rs`): one trait, a real
//! implementation that forwards to the actual Wayland proxy, and a
//! scripted recorder whose call log tests assert against directly.
//!
//! ## Why this exists
//!
//! Before this seam, `WaylandState`'s shift-reset and shift-viewport
//! paths called `WpViewport::set_source` / `set_destination` directly.
//! Every existing test therefore pinned only the *arithmetic* feeding
//! those calls (offsets, geometry) — never the fact that a protocol
//! request was actually issued. Reverting the production call site
//! (e.g. deleting the `set_source(-1, -1, -1, -1)` line from
//! `WaylandState::reset_shift`) left every test green. Routing these
//! requests through `WaylandOps` lets a recorder assert on the call log
//! produced by the *same* orchestration functions `WaylandState` calls
//! in production, without opening a real Wayland connection — there is
//! no compositor in the test/sandbox environment, and constructing a
//! real `WaylandState` is not viable in tests (its SCTK fields —
//! `CompositorState`, `Shm`, `OutputState`, `LayerShell`,
//! `RegistryState`, `QueueHandle<WaylandState>` — all require a live
//! `wayland_client::Connection` to bind).
//!
//! ## Scope
//!
//! `wp_viewport` requests (create/bind, `set_source`, `set_destination`)
//! migrated first. This pass (test-seam #55, Task 2) adds the
//! screensaver's double-buffered `wl_shm` pool: `RawPool::new` and both
//! `RawPool::create_buffer` calls now go through [`WaylandOps::create_shm_pool`]
//! and [`WaylandOps::pool_create_buffer`], via the shared
//! [`create_screensaver_buffers`] orchestration `complete_screensaver_show`
//! and recorder tests both call directly — no free-function/method-body
//! duplication for tests to pin while production silently stops calling
//! it (see `state.rs`'s `ViewportStateView` docs for why that shape is
//! forbidden). Real `WlBuffer`/`RawPool` access for the per-frame mmap
//! writes and `wl_surface.attach` calls stays behind [`real_pool_with_region_mut`] /
//! [`real_buffer`] — real-only accessors that panic if ever called on a
//! recorder handle, exactly mirroring `RealWaylandOps`'s viewport
//! downcasts below. The black-transition attach/commit ordering is a
//! follow-up seam on the same trait (tracked separately) — this trait is
//! deliberately narrow, mirroring `VcpOps` rather than genericising the
//! whole of `WaylandState`.
//!
//! ### Why `create_shm_pool` / `pool_create_buffer` take no `Shm` / `QueueHandle` parameter
//!
//! `create_viewport` takes `&WpViewporter` / `&WlSurface` / `&QueueHandle`
//! as parameters because `wl_surface` varies per call. `Shm` and
//! `QueueHandle<WaylandState>` do not — both are connection-lifetime-stable
//! singletons `WaylandState` already holds unchanged for its entire life.
//! Requiring them as *trait-method* parameters would make the trait
//! untestable: `Shm`/`QueueHandle` can only be constructed from a live
//! `wayland_client::Connection` (see the module docs above), so no
//! recorder test could ever supply one. `RealWaylandOps` instead captures
//! its own clones of both at construction (`RealWaylandOps::new`,
//! called once from `connection::init`), so the trait methods stay
//! test-constructible: a `RecordingWaylandOps` needs neither.

use std::any::Any;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use smithay_client_toolkit::shm::raw::RawPool;
use smithay_client_toolkit::shm::{CreatePoolError, Shm};
use wayland_client::QueueHandle;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use super::state::{WaylandState, dual_buf_second_offset};

/// Opaque handle to a bound `wp_viewport` object. `WaylandState` stores
/// this (never a raw `WpViewport`) so every request against it must go
/// through a [`WaylandOps`] method. The real implementation wraps the
/// actual `WpViewport` proxy; the recorder implementation is an
/// identity tag with no live protocol object behind it.
pub(super) trait ViewportHandle: Send + Sync + fmt::Debug {
    /// Support for [`RealWaylandOps`] to recover the concrete
    /// `WpViewport` it wrapped. [`RecordingWaylandOps`] never
    /// downcasts — it only ever hands its own handles back to its own
    /// methods.
    fn as_any(&self) -> &dyn Any;
}

/// Opaque handle to an allocated `wl_shm_pool`-backed pool. Mirrors
/// [`ViewportHandle`]'s split: the real implementation wraps an actual
/// `RawPool` (behind a mutex — `RawPool::mmap`/`create_buffer` need
/// `&mut self`); the recorder implementation is an identity tag with
/// no live pool behind it. Never downcast in shared/test code — only
/// [`real_pool_with_region_mut`] (real-only, panics otherwise) and `RealWaylandOps`'s
/// own trait methods downcast this.
pub(super) trait PoolHandle: Send + Sync + fmt::Debug {
    fn as_any(&self) -> &dyn Any;
}

/// Opaque handle to a bound `wl_buffer`. Same split as [`PoolHandle`]:
/// real wraps the actual `WlBuffer` proxy, recorder is identity-only.
/// Real `WlBuffer` access is available ONLY through [`real_buffer`]
/// (real-only, panics otherwise) — used exclusively by `state.rs`'s
/// attach call sites and the `Dispatch<WlBuffer, ()>` release handler,
/// never by a recorder-backed test.
pub(super) trait BufferHandle: Send + Sync + fmt::Debug {
    fn as_any(&self) -> &dyn Any;
}

/// Abstract `wp_viewport` operations — real or scripted.
///
/// `Send + Sync` so `Arc<dyn WaylandOps>` can be shared with
/// `WaylandState` the same way `Arc<dyn VcpOps>` is shared with
/// `DdcciController` — no cross-thread sharing happens in practice
/// (the Wayland thread is single-threaded), but the bound costs
/// nothing and keeps the two seams symmetric.
pub(super) trait WaylandOps: Send + Sync {
    /// Bind a new `wp_viewport` to `wl_surface`. Callers MUST NOT call
    /// this twice for the same live surface — binding a second
    /// viewport to a surface that already has one is a protocol error
    /// (see `WaylandState::ensure_viewport`, the sole call site).
    fn create_viewport(
        &self,
        viewporter: &WpViewporter,
        wl_surface: &WlSurface,
        queue_handle: &QueueHandle<WaylandState>,
    ) -> Arc<dyn ViewportHandle>;

    /// `wp_viewport.set_source(x, y, width, height)`. Passing
    /// `(-1.0, -1.0, -1.0, -1.0)` is the protocol-documented way to
    /// unset a previously-set crop.
    fn viewport_set_source(
        &self,
        viewport: &dyn ViewportHandle,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    );

    /// `wp_viewport.set_destination(width, height)`.
    fn viewport_set_destination(&self, viewport: &dyn ViewportHandle, width: i32, height: i32);

    /// Allocate a `byte_len`-byte `wl_shm` pool. Real impl calls
    /// `RawPool::new` against the `Shm` global captured at
    /// [`RealWaylandOps::new`] time; see the module docs above for why
    /// `Shm` is not a parameter here.
    ///
    /// # Errors
    ///
    /// Returns the underlying `wl_shm` allocation error (real impl
    /// only — the recorder impl never fails).
    fn create_shm_pool(&self, byte_len: usize) -> Result<Arc<dyn PoolHandle>, CreatePoolError>;

    /// `wl_shm_pool.create_buffer` on `pool` at `offset`. Real impl
    /// forwards to `RawPool::create_buffer` against the `QueueHandle`
    /// captured at [`RealWaylandOps::new`] time; see the module docs
    /// above for why `QueueHandle` is not a parameter here.
    #[allow(clippy::too_many_arguments)]
    fn pool_create_buffer(
        &self,
        pool: &dyn PoolHandle,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: wl_shm::Format,
    ) -> Arc<dyn BufferHandle>;
}

// ── RealWaylandOps — forwards to the live `WpViewport` proxy ──────────────────

/// Real handle: wraps the actual `WpViewport` proxy bound to a live
/// surface.
#[derive(Debug)]
struct RealViewportHandle(WpViewport);

impl ViewportHandle for RealViewportHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Real handle: wraps the actual `RawPool` behind a mutex — `RawPool`'s
/// `mmap`/`create_buffer` both need `&mut self`, but [`PoolHandle`]
/// (like [`ViewportHandle`]) is shared via `Arc<dyn PoolHandle>`.
/// There is no cross-thread contention in practice (single Wayland
/// thread) — the mutex exists purely to get interior mutability
/// through the trait-object boundary.
struct RealPoolHandle(StdMutex<RawPool>);

impl fmt::Debug for RealPoolHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealPoolHandle").finish_non_exhaustive()
    }
}

impl PoolHandle for RealPoolHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl RealPoolHandle {
    /// Real-only accessor: run `f` against the `[offset, offset+len)`
    /// byte region of the pool's mmap. Only ever called from
    /// production code in `state.rs` (`on_mpv_wakeup`,
    /// `on_transition_tick`) via [`real_pool_with_region_mut`] — never from a
    /// recorder-backed test, which never holds a `RealPoolHandle` to
    /// downcast to.
    ///
    /// # Panics
    ///
    /// Panics if `[offset, offset+len)` is out of the pool's bounds,
    /// or if the internal mutex is poisoned.
    fn with_region_mut<R>(&self, offset: usize, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let mut guard = self.0.lock().expect("RawPool mutex poisoned");
        f(&mut guard.mmap()[offset..offset + len])
    }
}

/// Real handle: wraps the actual `WlBuffer` proxy bound to a pool.
#[derive(Debug)]
struct RealBufferHandle(WlBuffer);

impl BufferHandle for RealBufferHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Real-only: downcast an opaque pool handle back to the concrete
/// `RawPool` it wraps, and run `f` against `[offset, offset+len)` of
/// its mmap. The sole callers (`state.rs`'s `on_mpv_wakeup` and
/// `on_transition_tick`) only ever run in production, where
/// `RealWaylandOps` created every pool handle in play.
///
/// # Panics
///
/// Panics if `handle` is not a `RealPoolHandle` (i.e. this is called
/// against a recorder handle outside production) or if the region is
/// out of bounds.
pub(super) fn real_pool_with_region_mut<R>(
    handle: &dyn PoolHandle,
    offset: usize,
    len: usize,
    f: impl FnOnce(&mut [u8]) -> R,
) -> R {
    handle
        .as_any()
        .downcast_ref::<RealPoolHandle>()
        .expect("real_pool_with_region_mut called on a non-Real pool handle outside production")
        .with_region_mut(offset, len, f)
}

/// Real-only: downcast an opaque buffer handle back to the concrete
/// `WlBuffer` it wraps. Panics if `handle` isn't a `RealBufferHandle`
/// — the sole callers (`state.rs`'s attach call sites and the
/// `Dispatch<WlBuffer, ()>` release handler) only ever run in
/// production, where `RealWaylandOps` created every buffer handle in
/// play.
///
/// # Panics
///
/// Panics if `handle` is not a `RealBufferHandle`.
pub(super) fn real_buffer(handle: &dyn BufferHandle) -> &WlBuffer {
    &handle
        .as_any()
        .downcast_ref::<RealBufferHandle>()
        .expect("real_buffer called on a non-Real buffer handle outside production")
        .0
}

/// Production `WaylandOps` — the viewport methods forward straight to
/// the real `WpViewport` proxy and hold no state of their own (each
/// call receives the globals it needs — `viewporter`, `queue_handle` —
/// from `WaylandState`). The pool/buffer methods DO hold state: `shm`
/// and `queue_handle` are connection-lifetime-stable singletons
/// captured once at construction — see the module docs above for why
/// (test-constructibility of [`WaylandOps::create_shm_pool`] /
/// [`WaylandOps::pool_create_buffer`]).
pub(super) struct RealWaylandOps {
    shm: Shm,
    queue_handle: QueueHandle<WaylandState>,
}

impl RealWaylandOps {
    /// `shm` and `queue_handle` are clones of `WaylandState`'s own
    /// fields, taken once in `connection::init` before those originals
    /// are moved into `WaylandState::new`. Cloning a `Shm`/`QueueHandle`
    /// is cheap (both are thin proxy/handle wrappers) and does not
    /// duplicate any live protocol resource.
    pub(super) fn new(shm: Shm, queue_handle: QueueHandle<WaylandState>) -> Self {
        Self { shm, queue_handle }
    }
}

impl WaylandOps for RealWaylandOps {
    fn create_viewport(
        &self,
        viewporter: &WpViewporter,
        wl_surface: &WlSurface,
        queue_handle: &QueueHandle<WaylandState>,
    ) -> Arc<dyn ViewportHandle> {
        Arc::new(RealViewportHandle(viewporter.get_viewport(
            wl_surface,
            queue_handle,
            (),
        )))
    }

    fn viewport_set_source(
        &self,
        viewport: &dyn ViewportHandle,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) {
        let handle = viewport
            .as_any()
            .downcast_ref::<RealViewportHandle>()
            .expect("RealWaylandOps always receives a RealViewportHandle it created");
        handle.0.set_source(x, y, width, height);
    }

    fn viewport_set_destination(&self, viewport: &dyn ViewportHandle, width: i32, height: i32) {
        let handle = viewport
            .as_any()
            .downcast_ref::<RealViewportHandle>()
            .expect("RealWaylandOps always receives a RealViewportHandle it created");
        handle.0.set_destination(width, height);
    }

    fn create_shm_pool(&self, byte_len: usize) -> Result<Arc<dyn PoolHandle>, CreatePoolError> {
        let pool = RawPool::new(byte_len, &self.shm)?;
        Ok(Arc::new(RealPoolHandle(StdMutex::new(pool))))
    }

    fn pool_create_buffer(
        &self,
        pool: &dyn PoolHandle,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: wl_shm::Format,
    ) -> Arc<dyn BufferHandle> {
        let real_pool = pool
            .as_any()
            .downcast_ref::<RealPoolHandle>()
            .expect("RealWaylandOps always receives a RealPoolHandle it created");
        let mut guard = real_pool.0.lock().expect("RawPool mutex poisoned");
        let buffer = guard.create_buffer(
            offset,
            width,
            height,
            stride,
            format,
            (),
            &self.queue_handle,
        );
        Arc::new(RealBufferHandle(buffer))
    }
}

// ── RecordingWaylandOps — scripted, for tests ──────────────────────────────────

/// Recorder handle: carries only an identity, no live protocol object
/// — there is no compositor in tests, so nothing real to wrap.
#[derive(Debug)]
pub(super) struct RecordingViewportHandle {
    id: u64,
}

impl ViewportHandle for RecordingViewportHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Recorder handle: carries only an identity — no live `RawPool`
/// behind it (there is no compositor in tests to allocate one
/// against).
#[derive(Debug)]
pub(super) struct RecordingPoolHandle {
    id: u64,
}

impl PoolHandle for RecordingPoolHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Recorder handle: carries only an identity — no live `WlBuffer`
/// behind it. `id` is never read back out today (no test currently
/// needs to distinguish one recorded buffer handle from another by
/// identity — only the call log's `#{id}` text matters), but is kept
/// for structural symmetry with `RecordingViewportHandle` /
/// `RecordingPoolHandle` and any future test that does.
#[derive(Debug)]
pub(super) struct RecordingBufferHandle {
    #[allow(dead_code)]
    id: u64,
}

impl BufferHandle for RecordingBufferHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Scripted [`WaylandOps`] for tests. Records every call (formatted,
/// FIFO) so a test can assert the exact sequence the production
/// orchestration issued through `Arc<dyn WaylandOps>` — mirrors
/// `dormant_displays::vcp_ops::FakeVcp`'s `call_log` /
/// `take_call_log`.
#[derive(Debug, Default)]
pub(super) struct RecordingWaylandOps {
    next_id: StdMutex<u64>,
    call_log: StdMutex<Vec<String>>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RecordingWaylandOps {
    /// Create an empty recorder.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh recorder handle identity without going
    /// through `create_viewport` — production only calls
    /// `create_viewport` with a real `WpViewporter` / `WlSurface` /
    /// `QueueHandle<WaylandState>`, none of which a compositor-free
    /// test can construct. This mirrors `WaylandState.viewport`
    /// already being `Some` (bound by a prior real `ensure_viewport`
    /// call) at the point the transition orchestration under test
    /// runs.
    pub(super) fn seed_viewport(&self) -> Arc<dyn ViewportHandle> {
        self.next_handle()
    }

    fn next_handle(&self) -> Arc<dyn ViewportHandle> {
        Arc::new(RecordingViewportHandle {
            id: self.alloc_id(),
        })
    }

    /// Allocate a fresh identity — shared by every recorder handle kind
    /// (viewport, pool, buffer) so call-log entries across kinds are
    /// distinguishable by a single incrementing counter.
    fn alloc_id(&self) -> u64 {
        let mut next_id = self.next_id.lock().unwrap();
        let id = *next_id;
        *next_id += 1;
        id
    }

    /// Drain the call log (FIFO).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub(super) fn take_call_log(&self) -> Vec<String> {
        std::mem::take(&mut self.call_log.lock().unwrap())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl WaylandOps for RecordingWaylandOps {
    fn create_viewport(
        &self,
        _viewporter: &WpViewporter,
        _wl_surface: &WlSurface,
        _queue_handle: &QueueHandle<WaylandState>,
    ) -> Arc<dyn ViewportHandle> {
        let handle = self.next_handle();
        let id = handle
            .as_any()
            .downcast_ref::<RecordingViewportHandle>()
            .expect("next_handle always returns a RecordingViewportHandle")
            .id;
        self.call_log
            .lock()
            .unwrap()
            .push(format!("create_viewport(#{id})"));
        handle
    }

    fn viewport_set_source(
        &self,
        viewport: &dyn ViewportHandle,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) {
        let id = viewport
            .as_any()
            .downcast_ref::<RecordingViewportHandle>()
            .map_or(u64::MAX, |h| h.id);
        self.call_log.lock().unwrap().push(format!(
            "viewport_set_source(#{id}, {x}, {y}, {width}, {height})"
        ));
    }

    fn viewport_set_destination(&self, viewport: &dyn ViewportHandle, width: i32, height: i32) {
        let id = viewport
            .as_any()
            .downcast_ref::<RecordingViewportHandle>()
            .map_or(u64::MAX, |h| h.id);
        self.call_log.lock().unwrap().push(format!(
            "viewport_set_destination(#{id}, {width}, {height})"
        ));
    }

    fn create_shm_pool(&self, byte_len: usize) -> Result<Arc<dyn PoolHandle>, CreatePoolError> {
        let id = self.alloc_id();
        self.call_log
            .lock()
            .unwrap()
            .push(format!("create_shm_pool(#{id}, {byte_len})"));
        Ok(Arc::new(RecordingPoolHandle { id }))
    }

    fn pool_create_buffer(
        &self,
        pool: &dyn PoolHandle,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: wl_shm::Format,
    ) -> Arc<dyn BufferHandle> {
        let pool_id = pool
            .as_any()
            .downcast_ref::<RecordingPoolHandle>()
            .map_or(u64::MAX, |h| h.id);
        let id = self.alloc_id();
        self.call_log.lock().unwrap().push(format!(
            "create_buffer(#{id}, pool=#{pool_id}, offset={offset}, {width}x{height}, stride={stride}, {format:?})"
        ));
        Arc::new(RecordingBufferHandle { id })
    }
}

// ── Shared screensaver dual-buffer orchestration (prod + recorder tests) ──
//
// `complete_screensaver_show` (state.rs) and the recorder test below
// call this SAME function — there is no free-function/method-body
// duplication for a test to pin while production silently stops
// calling it (the exact shape `state.rs`'s `ViewportStateView` docs
// forbid). Before this pass, the pool byte length and the two
// `create_buffer` offsets were only proven by
// `create_dual_buffers_core`'s closure seam (PR #57) — a test could
// stay green even if `complete_screensaver_show` stopped calling
// `RawPool::new`/`RawPool::create_buffer` at all, as long as the
// closure-driving test still called the closure directly. Retired
// (deleted) alongside this function landing.

/// Build the screensaver's double-buffered `wl_shm` pool: a single
/// `2 * stride * height`-byte pool (via [`WaylandOps::create_shm_pool`])
/// holding two XRGB8888 buffers at offsets `0` and `stride * height`
/// (via [`WaylandOps::pool_create_buffer`], twice) — `buf0` and `buf1`
/// so mpv can ping-pong writes without overwriting a buffer the
/// compositor is still reading. `width`/`height` are the buffer
/// dimensions in pixels; `stride` is bytes-per-row (already validated
/// by the caller — see `complete_screensaver_show`'s `stride` overflow
/// check).
///
/// # Errors
///
/// Returns the underlying `wl_shm` allocation error (real impl only —
/// the recorder impl never fails), or `CreatePoolError::Create` if
/// `2 * stride * height` overflows `u64` — physically unreachable for
/// any real display size, but reported gracefully rather than
/// panicking since this runs inside the Wayland dispatch callback.
///
/// # Panics
///
/// Panics if the second buffer's offset (`stride * height`, cast to
/// `i32`) overflows — physically unreachable for any real display
/// size (mirrors the existing `i32::try_from(...).expect(...)`
/// invariant this function replaces at the `create_dual_buffers` call
/// site).
/// The screensaver's allocated pool handle plus its two buffer
/// handles (`buf0` at offset 0, `buf1` at offset `stride * height`).
pub(super) type ScreensaverBuffers = (Arc<dyn PoolHandle>, [Arc<dyn BufferHandle>; 2]);

pub(super) fn create_screensaver_buffers(
    ops: &dyn WaylandOps,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<ScreensaverBuffers, CreatePoolError> {
    let buf1_offset_bytes = dual_buf_second_offset(stride, height);
    let pool_byte_len_bytes = buf1_offset_bytes.checked_mul(2).ok_or_else(|| {
        CreatePoolError::Create(io::Error::other(
            "pool byte length (2 * stride * height) overflowed u64",
        ))
    })?;
    let pool_byte_len =
        usize::try_from(pool_byte_len_bytes).expect("pool byte length must fit in usize");
    let pool = ops.create_shm_pool(pool_byte_len)?;
    let fmt = wl_shm::Format::Xrgb8888;
    let w = width.cast_signed();
    let h = height.cast_signed();
    let stride_i32 = stride.cast_signed();
    let buf0 = ops.pool_create_buffer(pool.as_ref(), 0, w, h, stride_i32, fmt);
    let offset1 = i32::try_from(buf1_offset_bytes).expect("second buffer offset must fit in i32");
    let buf1 = ops.pool_create_buffer(pool.as_ref(), offset1, w, h, stride_i32, fmt);
    Ok((pool, [buf0, buf1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_logs_seeded_handle_set_source_in_order() {
        let ops = RecordingWaylandOps::new();
        let viewport = ops.seed_viewport();
        ops.viewport_set_destination(viewport.as_ref(), 100, 200);
        ops.viewport_set_source(viewport.as_ref(), 1.0, 2.0, 3.0, 4.0);
        assert_eq!(
            ops.take_call_log(),
            vec![
                "viewport_set_destination(#0, 100, 200)".to_string(),
                "viewport_set_source(#0, 1, 2, 3, 4)".to_string(),
            ]
        );
    }

    #[test]
    fn create_screensaver_buffers_allocates_pool_and_two_buffers() {
        // RED (test-seam #55, Task 2): the production dual-buffer pool
        // setup for a 640x480 screensaver frame (stride 2560, no
        // shift-margin padding) must go through `WaylandOps` so a
        // recorder can prove `RawPool::new`'s size AND both real
        // `create_buffer` call sites, not just the `create_dual_buffers_core`
        // closure's arithmetic (PR #57's ad-hoc seam).
        let ops = RecordingWaylandOps::new();
        let (width, height, stride) = (640u32, 480u32, 2560u32);
        let (_pool, _buffers) = create_screensaver_buffers(&ops, width, height, stride)
            .expect("pool creation must succeed");
        let expected_pool_len = 2u64 * u64::from(stride) * u64::from(height);
        let expected_buf1_offset = u64::from(stride) * u64::from(height);
        let log = ops.take_call_log();
        assert_eq!(
            log,
            vec![
                format!("create_shm_pool(#0, {expected_pool_len})"),
                format!(
                    "create_buffer(#1, pool=#0, offset=0, {width}x{height}, stride={stride}, Xrgb8888)"
                ),
                format!(
                    "create_buffer(#2, pool=#0, offset={expected_buf1_offset}, {width}x{height}, stride={stride}, Xrgb8888)"
                ),
            ],
            "must allocate a pool of 2*stride*height bytes and issue exactly two \
             create_buffer requests at offsets [0, stride*height], both XRGB8888 \
             with identical width/height/stride"
        );
    }

    #[test]
    fn create_screensaver_buffers_reports_pool_size_overflow_as_err() {
        // RED (fix round 1): `2 * stride * height` overflowing `u64`
        // must surface as `Err(CreatePoolError::Create(_))` -- a
        // graceful `CmdFailure`-style path -- not a panic. Pre-migration
        // this was `checked_mul(2)` feeding a `CmdFailure`; the
        // migration to `WaylandOps` briefly regressed it to
        // `.expect(...)`, which aborts the process because this runs
        // inside the Wayland dispatch callback. `stride == height ==
        // u32::MAX` makes `stride * height` (as u64) exceed
        // `u64::MAX / 2`, so `checked_mul(2)` overflows on the FIRST
        // checked op this function performs (the `i32::try_from` on
        // the offset, later in the function, is never reached).
        let ops = RecordingWaylandOps::new();
        let (width, height, stride) = (640u32, u32::MAX, u32::MAX);
        let result = create_screensaver_buffers(&ops, width, height, stride);
        assert!(
            matches!(result, Err(CreatePoolError::Create(_))),
            "expected Err(CreatePoolError::Create(_)) for an overflowing pool size, got {result:?}"
        );
        assert!(
            ops.take_call_log().is_empty(),
            "must fail before issuing any WaylandOps call"
        );
    }

    #[test]
    fn take_call_log_drains_fifo() {
        let ops = RecordingWaylandOps::new();
        let viewport = ops.seed_viewport();
        ops.viewport_set_source(viewport.as_ref(), -1.0, -1.0, -1.0, -1.0);
        assert_eq!(ops.take_call_log().len(), 1);
        assert!(
            ops.take_call_log().is_empty(),
            "log must drain, not accumulate"
        );
    }
}
