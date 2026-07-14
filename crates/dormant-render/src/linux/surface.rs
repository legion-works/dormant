//! Layer-surface creation, black-buffer attachment, and shm fallback.
//!
//! These helpers are pure Wayland-side work — no calloop, no threading.
//! They take a concrete `&WaylandState` reference (rather than a
//! generic `D: Dispatch<...>`) so the SCTK trait bounds on
//! `CompositorState::create_surface`, `RawPool::create_buffer`, etc.
//! are satisfied at the call site.

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface,
};

use std::sync::Arc;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_surface::WlSurface;

use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::linux::state::WaylandState;
use crate::linux::wayland_ops::ViewportHandle;

/// Opaque black in `u32` ARGB host order, matching
/// `WpSinglePixelBufferManagerV1::create_u32_rgba_buffer`.
pub(super) const OPAQUE_BLACK_U32: u32 = 0xFF00_0000;

/// Namespace surfaced in `wayland-info` / `wayland-debug`.
pub(super) const LAYER_NAMESPACE: &str = "dormant";

/// Pixel format for every shm buffer this crate hands to the compositor.
///
/// **`XRGB8888` is intentional, not a typo for `ARGB8888`.**  mpv's
/// `bgr0` SW format writes bytes `[B, G, R, X]` with `X = 0x00`
/// (verified by the spike's `[ff,00,ff,00]` magenta pixel dump).
/// Under `ARGB8888` the compositor reads that 4th byte as ALPHA → an
/// alpha=0 frame composites fully transparent (the desktop shows
/// through — the screensaver renders invisible).  `XRGB8888` declares
/// "the 4th byte is ignored"; the same byte stream is correct opaque
/// content either way.  The black shm fallback uses the same format
/// for symmetry — opaque content, no alpha channel to manage.
pub(super) const SHM_PIXEL_FORMAT: wayland_client::protocol::wl_shm::Format =
    wayland_client::protocol::wl_shm::Format::Xrgb8888;

/// Create a fullscreen-anchored Overlay layer surface on `target_output`.
///
/// The returned [`LayerSurface`] is in the *initial* state — a single
/// `commit()` will trigger a `configure` from the compositor; the buffer
/// is attached after that, in [`attach_single_pixel_black`] or
/// [`create_shm_black_buffer`].
pub(super) fn create_layer_surface(
    layer_shell: &LayerShell,
    compositor_state: &CompositorState,
    target_output: &WlOutput,
    state: &WaylandState,
) -> LayerSurface {
    let qh = state.queue_handle.clone();
    let surface = compositor_state.create_surface(&qh);
    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some(LAYER_NAMESPACE),
        Some(target_output),
    );
    layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    // -1 = ignore exclusive zones from other layers; the overlay sits
    // above everything.
    layer_surface.set_exclusive_zone(-1);
    // Exclusive = the compositor routes keyboard input to us.  This is
    // the wake grab; the daemon's input latch fires on the first key.
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer_surface.commit();
    layer_surface
}

/// Attach a 1×1 opaque-black buffer via `wp_viewporter::set_destination`
/// so the compositor scales it to fill the configured `width × height`.
///
/// Routes the viewport bind + `set_destination` through
/// `state.wayland_ops` (not `Dispatch`/SCTK-required — see the
/// `wayland_ops` module docs) — this is the ONE call site that binds a
/// viewport before `WaylandState::ensure_viewport` has ever run (the
/// very first black show on a fresh surface); the returned handle is
/// cached into `state.viewport` by the caller so every later request
/// reuses it via `ensure_viewport`.
#[must_use]
pub(super) fn attach_single_pixel_black(
    single_pixel_manager: &crate::linux::state::SinglePixelBufferManager,
    viewporter: &WpViewporter,
    wl_surface: &WlSurface,
    width: u32,
    height: u32,
    state: &WaylandState,
) -> (WlBuffer, Arc<dyn ViewportHandle>) {
    let qh = state.queue_handle.clone();
    // Per-channel u32 values scaled over the full u32 range — NOT a
    // packed ARGB8888 pixel.  The `OPAQUE_BLACK_U32` constant is for the
    // shm path (a packed pixel written into the buffer); passing it as the
    // `r` channel here would give ~99.6% red.
    let buffer = single_pixel_manager.create_u32_rgba_buffer(0, 0, 0, u32::MAX, &qh, ());
    let viewport = state
        .wayland_ops
        .create_viewport(viewporter, wl_surface, &qh);
    state.wayland_ops.viewport_set_destination(
        viewport.as_ref(),
        width.cast_signed(),
        height.cast_signed(),
    );
    wl_surface.attach(Some(&buffer), 0, 0);
    wl_surface.commit();
    (buffer, viewport)
}

/// Create an opaque-black shm buffer (fallback path when the staging
/// globals — single-pixel-buffer + viewporter — are unavailable).
///
/// Fills the buffer with `0xFF00_0000` (ARGB host order) so the rendered
/// pixels are opaque black.  Returns the [`WlBuffer`] ready to attach.
pub(super) fn create_shm_black_buffer(
    width: u32,
    height: u32,
    state: &WaylandState,
) -> Result<WlBuffer, String> {
    let stride = width.cast_signed() * 4;
    let byte_len =
        (usize::try_from(stride).map_err(|e| format!("stride cast: {e}"))?) * (height as usize);
    let mut pool = smithay_client_toolkit::shm::raw::RawPool::new(byte_len, &state.shm_state)
        .map_err(|e| format!("RawPool::new: {e}"))?;
    {
        let mmap = pool.mmap();
        let pixel = OPAQUE_BLACK_U32.to_ne_bytes();
        for row in 0..height as usize {
            let row_start = row * (width as usize) * 4;
            for col in 0..(width as usize) {
                let offset = row_start + col * 4;
                mmap[offset..offset + 4].copy_from_slice(&pixel);
            }
        }
    }
    let qh = state.queue_handle.clone();
    let buffer = pool.create_buffer(
        0,
        width.cast_signed(),
        height.cast_signed(),
        stride,
        SHM_PIXEL_FORMAT,
        (),
        &qh,
    );
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned at `XRGB8888` so the alpha-trap regression cannot
    /// silently return.  See the const's doc for why ARGB is wrong.
    #[test]
    fn shm_pixel_format_is_xrgb_8888_to_ignore_mpv_bgr0_pad_byte() {
        assert_eq!(
            SHM_PIXEL_FORMAT,
            wayland_client::protocol::wl_shm::Format::Xrgb8888
        );
    }
}

// (SinglePixelBufferManager alias lives in `state.rs` for callers that
// want the short name.)
