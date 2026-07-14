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
//! Only `wp_viewport` requests (create/bind, `set_source`,
//! `set_destination`) migrate in this pass. Shm pool/buffer allocation
//! and the black-transition attach/commit ordering are follow-up seams
//! on the same `WaylandOps` trait (tracked separately) — this trait is
//! deliberately narrow, mirroring `VcpOps` rather than genericising the
//! whole of `WaylandState`.

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use wayland_client::QueueHandle;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use super::state::WaylandState;

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

/// Production `WaylandOps` — every method forwards straight to the
/// real `WpViewport` proxy. Holds no state of its own: each call
/// receives the globals it needs (`viewporter`, `queue_handle`) from
/// `WaylandState`, the only place that owns them.
pub(super) struct RealWaylandOps;

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
        let mut next_id = self.next_id.lock().unwrap();
        let id = *next_id;
        *next_id += 1;
        Arc::new(RecordingViewportHandle { id })
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
