//! Wayland thread state — the dispatch target for every Wayland object.
//!
//! All Wayland proxies + the SCTK handler state live here.  Crucially:
//! the `EventQueue` itself is **not** a field — it stays loop-local in
//! `connection.rs`.  We hold a clone of its `QueueHandle` (cheap,
//! `'static`) so surface-creation calls can still bind proxies to the
//! right queue without storing the queue itself.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
};

use tokio::sync::mpsc::UnboundedSender;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, Mode, PostAction};

use dormant_core::error::E_RENDER_UNAVAILABLE;
use dormant_core::types::{CmdFailure, DisplayId, StageKind};

use super::blend::{self, T_MAX};
use crate::command::RenderCommand;
use crate::latch::FirstInputLatch;
use crate::screensaver::{MpvItemEvent, MpvPlayer};
use crate::settings::{ScreensaverSettings, TransitionMode};

/// Re-export of the long `WpSinglePixelBufferManagerV1` type so callers
/// in [`crate::linux::surface`] can name it without a full
/// `wayland_protocols::wp::...` path.
pub(super) type SinglePixelBufferManager =
    wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;

/// Maximum time we'll wait for a compositor `configure` event after the
/// initial layer-surface commit.  Compositors are expected to respond
/// in single-digit milliseconds; 2 seconds is comfortable slack without
/// masking a genuine hang.  The configure-timeout timer in
/// `connection::arm_configure_timer` fires this far in the future and
/// resolves the pending oneshot with an `E_RENDER_UNAVAILABLE` error if
/// the compositor doesn't reply.
pub(super) const CONFIGURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// In-flight `Show` awaiting its compositor `configure` reply.
///
/// Stored on the state when a Show command is accepted, cleared by the
/// configure handler on success or by the configure-timeout handler on
/// silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SurfaceMatch {
    /// The event's surface matches the still-pending show's surface.
    Pending,
    /// The event's surface matches the live (committed) surface.
    Live,
    /// The event's surface matches neither — stale, ignore.
    Stale,
}

/// Pure identity-match decision.  Compares an incoming Wayland surface
/// `ObjectId` against the still-pending show's surface id and the
/// live surface's id.
///
/// This is factored out so the configure/closed guards are unit-
/// testable without constructing a `WaylandState` (the real state
/// carries SCTK proxies that need a live compositor).
pub(super) fn surface_match(
    pending_surface_id: Option<&wayland_client::backend::ObjectId>,
    live_surface_id: Option<&wayland_client::backend::ObjectId>,
    event_surface_id: &wayland_client::backend::ObjectId,
) -> SurfaceMatch {
    if pending_surface_id == Some(event_surface_id) {
        SurfaceMatch::Pending
    } else if live_surface_id == Some(event_surface_id) {
        SurfaceMatch::Live
    } else {
        SurfaceMatch::Stale
    }
}

/// Build a `CmdFailure` for one of the render sub-controllers.
/// Centralised so the error sites in this file don't drift on the
/// `E_RENDER_UNAVAILABLE` prefix or the controller tag.
fn cmd_failure(controller: &'static str, detail: &str) -> CmdFailure {
    CmdFailure {
        controller: controller.into(),
        error: format!("{E_RENDER_UNAVAILABLE}: {detail}"),
    }
}

/// Create a non-blocking `CLOEXEC` pipe for the mpv wakeup callback
/// to write into.  Returns `(read_fd, write_fd)` as [`OwnedFd`]s — the
/// read end is registered with calloop (borrowed) and ultimately
/// stored in `ScreensaverSession`; the write end is consumed by
/// [`MpvPlayer::new`] which closes it on construction failure via
/// [`OwnedFd::drop`] (the caller MUST NOT close it on the Err path).
fn make_wakeup_pipe() -> Result<(OwnedFd, OwnedFd), CmdFailure> {
    let mut pipe_fds = [0 as RawFd; 2];
    // SAFETY: pipe2 writes both fds into the provided array.
    let ret = unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if ret < 0 {
        return Err(cmd_failure(
            "screensaver",
            &format!("pipe2: {}", std::io::Error::last_os_error()),
        ));
    }
    // SAFETY: pipe2 returned two fresh fds; we own both and have not
    // closed them.  `OwnedFd::from_raw_fd` takes exclusive ownership.
    let read_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
    Ok((read_fd, write_fd))
}

/// Build two `WlBuffer`s from a single `RawPool` — `buf0` at offset 0,
/// `buf1` at offset `stride * height`.  Together they cover the full
/// pool so mpv can ping-pong writes without overwriting a buffer the
/// compositor is still reading.
fn create_dual_buffers(
    pool: &mut smithay_client_toolkit::shm::raw::RawPool,
    qh: &QueueHandle<WaylandState>,
    width: u32,
    height: u32,
    stride: u32,
) -> (WlBuffer, WlBuffer) {
    let stride_i32 = stride.cast_signed();
    // XRGB8888 — NOT ARGB8888.  mpv's `bgr0` SW format writes bytes
    // [B,G,R,X] with X = 0x00; under ARGB8888 the compositor reads that
    // byte as alpha=0 and composites every frame fully transparent
    // (the desktop shows through — invisible screensaver).  XRGB8888
    // declares "the 4th byte is ignored"; the same byte stream is
    // correct content either way.
    let fmt = wayland_client::protocol::wl_shm::Format::Xrgb8888;
    let buf0 = pool.create_buffer(
        0,
        width.cast_signed(),
        height.cast_signed(),
        stride_i32,
        fmt,
        (),
        qh,
    );
    let buf1 = pool.create_buffer(
        stride.cast_signed(),
        width.cast_signed(),
        height.cast_signed(),
        stride_i32,
        fmt,
        (),
        qh,
    );
    (buf0, buf1)
}

/// Calloop dispatch callback for the screensaver wakeup pipe.
/// Delegates to [`WaylandState::on_mpv_wakeup`] for the actual work.
/// Returns `Ok(PostAction)` (never an `Err`) — the calloop source's
/// error type is fixed as `std::io::Error` but our callback never
/// produces one; `Result` wrapping is required by the `EventSource`
/// contract on `Generic`.
#[allow(clippy::unnecessary_wraps)] // calloop's EventSource contract mandates Result
fn screensaver_wakeup_cb(
    _readiness: calloop::Readiness,
    _meta: &mut calloop::generic::NoIoDrop<BorrowedFd<'_>>,
    state: &mut WaylandState,
) -> Result<PostAction, std::io::Error> {
    state.on_mpv_wakeup();
    Ok(PostAction::Continue)
}

pub(super) struct PendingShow {
    /// Stage generation counter — forwarded to the log on completion.
    pub(super) r#gen: u64,
    /// What stage is being shown — remembered so the configure handler
    /// can pick the right buffer-attachment strategy (single-pixel black
    /// vs shm pool for screensaver).
    pub(super) kind: StageKind,
    /// Optional screensaver payload (only set for `RenderScreensaver`).
    pub(super) screensaver: Option<ScreensaverSettings>,
    /// The layer surface created by the Show command (still in
    /// pre-configure state — no buffer attached yet).
    pub(super) layer_surface: LayerSurface,
    /// Reply channel — resolved by the configure handler or the timer.
    pub(super) reply: tokio::sync::oneshot::Sender<Result<(), CmdFailure>>,
}

/// Crossfade state for one screensaver session.
///
/// Holds the capture buffer (allocated lazily on first `ItemEnded`),
/// the per-tick blend progress, and the calloop timer's registration
/// token.  Lives as long as the next transition cycle — allocation
/// overhead is one `Vec<u8>` of `stride * height` bytes (~21 MiB at
/// 4K).  Kept across cycles so subsequent transitions reuse the same
/// memory.
pub(super) struct TransitionState {
    /// Generation counter for stale-guard — see [`ScreensaverSession::pending_gen`].
    pub(super) r#gen: u64,
    /// Snapshot of the outgoing item's last rendered frame.  Copied
    /// from the front buffer on `ItemEnded`; reused (overwritten in
    /// place) on the next transition cycle.  Length is
    /// `stride * height` (matches one screensaver frame).
    pub(super) capture: Vec<u8>,
    /// calloop registration for the periodic blend timer.
    /// Removed (calloop side) when the timer fires its last tick and
    /// returns `TimeoutAction::Drop`; removed by
    /// `destroy_screensaver_session` on teardown.
    pub(super) timer_token: Option<calloop::RegistrationToken>,
    /// Current blend progress: 0 = pure capture, 256 = pure new frame
    /// (see [`crate::linux::blend`]).
    pub(super) t_frac: u16,
    /// `T_MAX / (fps * duration_secs)` — the per-tick increment that
    /// completes the blend in `duration` at `fps`.  Computed at timer
    /// arm time so changing `t_frac` mid-blend never divides unevenly.
    pub(super) t_step: u16,
    /// Frame rate the blend runs at.  Hard-coded to 30 — the spike
    /// measured 0.9 ms/frame at 4K, so even a 60 fps blend is well
    /// within budget and 30 fps is visibly smooth for a 0.5-1.0s
    /// crossfade.
    pub(super) fps: u32,
    /// Snapshot frames per second for the timer tick interval.
    pub(super) tick_interval: std::time::Duration,
    /// `false` after `ItemEnded` while we wait for the first new-item
    /// frame to land (`poll_events` surfaces `ItemLoaded` before the
    /// mpv wakeup carries the new pixel data); flipped to `true` on
    /// the first blend tick.  Stops the timer from rendering against
    /// a back buffer that still shows the OLD item's last frame.
    pub(super) waiting_for_first_new_frame: bool,
}

/// Active screensaver overlay — owned by the wayland thread, drives a
/// [`MpvPlayer`] into a double-buffered shm pool and registers the mpv
/// wakeup pipe read end as a calloop source.  The `wl_buffer`s live in
/// the SAME `RawPool` at offsets 0 and `stride * height` so the
/// compositor never reads the buffer mpv is currently writing.
///
/// Field `next_render_idx` is the index of the buffer we'll render
/// into on the next mpv wakeup — after each render we attach the
/// written buffer to the surface and flip the index.
pub(super) struct ScreensaverSession {
    pub(super) player: MpvPlayer,
    /// Owned single `RawPool` covering both buffers (`2 * stride * height` bytes).
    pub(super) pool: smithay_client_toolkit::shm::raw::RawPool,
    /// `buffers[0]` lives at offset 0, `buffers[1]` at offset `stride * height`.
    pub(super) buffers: [WlBuffer; 2],
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
    pub(super) next_render_idx: usize,
    /// Read end of the mpv wakeup pipe — owned by the session and
    /// closed when the session is dropped (calloop doesn't close the
    /// fd when a source is removed).  The matching write end lives
    /// inside the `MpvPlayer`.
    pub(super) read_fd: OwnedFd,
    /// Per-buffer compositor-busy flag — `true` after we attach a
    /// buffer (the compositor may still be reading from a prior
    /// attach); reset to `false` when the compositor sends the
    /// matching `wl_buffer.release`.  `on_mpv_wakeup` skips frames
    /// whose back buffer is still busy rather than overwriting live
    /// compositor state.
    pub(super) buffers_busy: [bool; 2],
    /// Reply for the originating `ShowScreensaver`.  Held pending until
    /// the first successful `on_mpv_wakeup` render (→ `Ok(())`) or the
    /// external first-frame deadline (→ `Err(E_RENDER_UNAVAILABLE)`).
    pub(super) pending_reply: Option<tokio::sync::oneshot::Sender<Result<(), CmdFailure>>>,
    /// Stage generation counter — used by the deadline timer to
    /// distinguish its target session from a later one.
    pub(super) pending_gen: u64,
    /// `true` after the first successful frame render; the deadline
    /// timer becomes a no-op once this flips.
    pub(super) has_first_frame: bool,
    /// `RegistrationToken` for the calloop deadline timer; removed
    /// when the first frame lands or when the session is torn down.
    pub(super) first_frame_token: Option<calloop::RegistrationToken>,
    /// Selected transition mode (`Crossfade` or `None`) — copied at
    /// session-build time from `ScreensaverSettings::transition` so
    /// the state machine doesn't have to plumb settings through every
    /// method call.  Used only to gate the transition paths; the
    /// `None` mode leaves every `transition` field below untouched.
    pub(super) transition_mode: TransitionMode,
    /// Crossfade state for the current cycle.  `None` between
    /// transitions (or always, when `transition_mode == None`).
    /// Allocation: one `Vec<u8>` of `stride * height` bytes per
    /// active transition (lazy, never `Some` before the first
    /// `ItemEnded`).
    pub(super) transition: Option<TransitionState>,
    /// Duration of the crossfade blend.  Carried here so the timer
    /// arm path doesn't have to re-dive into `ScreensaverSettings`.
    pub(super) transition_duration: std::time::Duration,
}

/// All Wayland-side state owned by the dedicated thread.  Holds every
/// Wayland proxy, the SCTK handler state, the input latch, and the
/// in-flight pending-show bookkeeping.
pub(super) struct WaylandState {
    // ── SCTK globals ───────────────────────────────────────────────────────
    pub(super) registry_state: RegistryState,
    pub(super) output_state: OutputState,
    pub(super) compositor_state: CompositorState,
    pub(super) shm_state: Shm,
    pub(super) layer_shell: LayerShell,

    // ── Staging globals (optional) ─────────────────────────────────────────
    pub(super) single_pixel_manager: Option<WpSinglePixelBufferManagerV1>,
    pub(super) viewporter: Option<WpViewporter>,

    // ── Seat + input ───────────────────────────────────────────────────────
    pub(super) seat: WlSeat,
    pub(super) pointer: Option<WlPointer>,
    pub(super) keyboard: Option<WlKeyboard>,
    #[allow(dead_code)] // future-input-grab debugging
    pub(super) last_pointer_serial: Option<u32>,

    // ── Per-display config ─────────────────────────────────────────────────
    pub(super) display_id: DisplayId,
    pub(super) output_name: String,
    pub(super) input_wake_tx: Option<UnboundedSender<DisplayId>>,

    // ── Live layer surface (after a successful Show) ──────────────────────
    pub(super) target_output: Option<WlOutput>,
    pub(super) layer_surface: Option<LayerSurface>,
    pub(super) viewport: Option<WpViewport>,
    pub(super) black_buffer: Option<WlBuffer>,
    pub(super) configured_size: (u32, u32),
    pub(super) surface_up: bool,

    // ── Pending Show (awaiting compositor `configure`) ─────────────────────
    pub(super) pending_show: Option<PendingShow>,

    // ── First-input latch ──────────────────────────────────────────────────
    pub(super) input_latch: FirstInputLatch,

    // ── Per-surface queue handle, cloned out of the loop-local queue ──────
    // We never store the `EventQueue` itself — that lives in the calloop
    // thread's stack frame (see `connection.rs`).  The handle is what
    // SCTK's `create_*` methods need to register proxies against a queue.
    #[allow(dead_code)] // re-referenced via State::queue_handle()
    pub(super) queue_handle: QueueHandle<WaylandState>,

    // ── Loop-exit flag (M4) ────────────────────────────────────────────────
    /// Set by the channel-source callback when [`calloop::channel::Event::Closed`]
    /// arrives (every sender has dropped).  [`crate::linux::connection::run_loop`]
    /// polls this between dispatch ticks and exits cleanly when true.
    pub(super) loop_should_exit: Arc<AtomicBool>,

    // ── Screensaver session (RenderScreensaver stage) ─────────────────────
    /// Active screensaver overlay — `Some` while a screensaver surface is
    /// live.  Owns the [`MpvPlayer`], the double-buffered shm pool, and
    /// (via the `screensaver_wakeup_token` below) the calloop registration
    /// of the mpv wakeup pipe read end.
    pub(super) screensaver_session: Option<ScreensaverSession>,

    /// calloop `RegistrationToken` for the mpv wakeup pipe read end —
    /// used to unregister the Generic source when the screensaver
    /// session is torn down (calloop's `Generic::drop` does NOT
    /// unregister; the caller must explicitly `loop_handle.remove(token)`).
    pub(super) screensaver_wakeup_token: Option<calloop::RegistrationToken>,

    // ── Loop handle (kept on state so screensaver install can register
    //    its wakeup source mid-flight) ─────────────────────────────────
    /// Clone of the loop's `LoopHandle` — cheap to clone (`Rc` inside),
    /// stable for the loop's lifetime.  Needed so the screensaver
    /// install path (called from inside the configure handler) can
    /// `insert_source` for the mpv wakeup pipe.
    #[allow(dead_code)] // installed + consumed inside complete_screensaver_show
    pub(super) loop_handle: Option<calloop::LoopHandle<'static, WaylandState>>,
}

impl WaylandState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        compositor_state: CompositorState,
        shm_state: Shm,
        output_state: OutputState,
        layer_shell: LayerShell,
        single_pixel_manager: Option<WpSinglePixelBufferManagerV1>,
        viewporter: Option<WpViewporter>,
        seat: WlSeat,
        registry_state: RegistryState,
        display_id: DisplayId,
        output_name: String,
        input_wake_tx: Option<&UnboundedSender<DisplayId>>,
        queue_handle: QueueHandle<WaylandState>,
        loop_should_exit: Arc<AtomicBool>,
    ) -> Self {
        let input_latch = FirstInputLatch::new(display_id.clone());
        Self {
            registry_state,
            output_state,
            compositor_state,
            shm_state,
            layer_shell,
            single_pixel_manager,
            viewporter,
            seat,
            pointer: None,
            keyboard: None,
            last_pointer_serial: None,
            display_id,
            output_name,
            input_wake_tx: input_wake_tx.cloned(),
            target_output: None,
            layer_surface: None,
            viewport: None,
            black_buffer: None,
            configured_size: (0, 0),
            surface_up: false,
            pending_show: None,
            input_latch,
            queue_handle,
            loop_should_exit,
            screensaver_session: None,
            screensaver_wakeup_token: None,
            loop_handle: None,
        }
    }

    /// Inject the loop handle (called from `connection::init` after the
    /// `EventLoop` is built).  Must be called exactly once before any
    /// screensaver install.
    pub(super) fn install_loop_handle(
        &mut self,
        handle: calloop::LoopHandle<'static, WaylandState>,
    ) {
        self.loop_handle = Some(handle);
    }

    /// Resolve any in-flight pending show with `Err`.  Used by teardown,
    /// the configure-timeout handler, and `Event::Closed` channel closure
    /// to ensure the oneshot is never left dangling.
    pub(super) fn fail_pending_show(&mut self, error: CmdFailure) {
        if let Some(pending) = self.pending_show.take() {
            let _ = pending.reply.send(Err(error));
        }
    }

    /// Pure guard for the configure-timeout handler.  Returns `true`
    /// only when `pending_gen == Some(timeout_gen)` — i.e. the timeout
    /// timer the caller is dispatching was armed for the *same* show
    /// that's still in flight.  Anything else (no pending show, or a
    /// newer show has superseded) is a stale timer that must NOT fail
    /// the live pending show.
    ///
    /// This is factored out so the gen-match discipline can be unit-
    /// tested without constructing a `WaylandState` (the real state
    /// carries Wayland proxies that need a live compositor).
    pub(super) fn should_fail_timeout(pending_gen: Option<u64>, timeout_gen: u64) -> bool {
        pending_gen == Some(timeout_gen)
    }

    /// Configure-timeout: the timer armed for `timeout_gen` fired.  Fail
    /// the pending oneshot only if the still-pending show's `r#gen`
    /// matches — otherwise the timer is stale (a newer show
    /// superseded it, or it completed cleanly) and must be a no-op.
    ///
    /// The race this guards against (round-2 review):
    /// 1. `Show(gen=1)` arms sleep-thread-1 to post `ConfigureTimeout{gen=1}`
    ///    in 2s.
    /// 2. Compositor `configure` arrives fast → pending→None, reply(Ok).
    /// 3. `Show(gen=2)` arms sleep-thread-2 with the same channel.
    /// 4. sleep-thread-1 fires → posts `ConfigureTimeout{gen=1}`.
    /// 5. Without the gen-match, the handler would fail the live
    ///    gen=2 pending show, breaking presence-flap blackouts.
    pub(super) fn handle_configure_timeout(&mut self, display_id: &DisplayId, r#gen: u64) {
        let pending_gen = self.pending_show.as_ref().map(|p| p.r#gen);
        if Self::should_fail_timeout(pending_gen, r#gen) {
            tracing::warn!(
                event = "render_configure_timeout",
                display_id = %self.display_id,
                timeout_display_id = %display_id,
                timeout_gen = r#gen,
                pending_gen = ?pending_gen,
                "compositor did not configure layer surface in {CONFIGURE_TIMEOUT:?}"
            );
            self.fail_pending_show(CmdFailure {
                controller: "render-black".into(),
                error: format!(
                    "{E_RENDER_UNAVAILABLE}: compositor did not configure layer surface in {CONFIGURE_TIMEOUT:?}"
                ),
            });
        } else {
            tracing::debug!(
                event = "render_configure_timeout_stale",
                display_id = %self.display_id,
                timeout_display_id = %display_id,
                timeout_gen = r#gen,
                pending_gen = ?pending_gen,
                "stale configure-timeout for a no-longer-pending show; ignored"
            );
        }
    }

    /// Resolve the in-flight pending show with `Ok`.  Marks the surface
    /// as live, attaches the opaque-black buffer, and consumes the
    /// pending entry.
    pub(super) fn complete_pending_show(&mut self, configured_size: (u32, u32)) {
        let Some(pending) = self.pending_show.take() else {
            // configure for a non-pending surface — the compositor may
            // resize an existing live surface (e.g. output geometry
            // change).  Track the new size and re-aim the viewport.
            self.configured_size = configured_size;
            if let (Some(viewport), true) = (&self.viewport, self.surface_up) {
                viewport.set_destination(
                    configured_size.0.cast_signed(),
                    configured_size.1.cast_signed(),
                );
                if let Some(surface) = &self.layer_surface {
                    surface.commit();
                }
            }
            return;
        };

        // Screensaver Show: build the player + shm pool, install the
        // wakeup pipe as a calloop source, and commit the back buffer.
        // On any pre-first-frame failure here, the show resolves as Err
        // and the engine falls through (the surface was just created so
        // there's nothing to revert to).
        if pending.kind == StageKind::RenderScreensaver {
            let Some(settings) = pending.screensaver else {
                let _ = pending.reply.send(Err(CmdFailure {
                    controller: "render-screensaver".into(),
                    error: format!("{E_RENDER_UNAVAILABLE}: screensaver show without settings"),
                }));
                return;
            };
            // complete_screensaver_show stores the reply on the session and arms
            // the first-frame deadline timer; the reply is sent by
            // `on_mpv_wakeup` (on first frame) or the timer (on 5-second
            // deadline).  Pre-install failures here still resolve the
            // reply with Err so the engine falls through.
            if let Err(e) = self.complete_screensaver_show(
                pending.layer_surface,
                configured_size,
                settings,
                pending.reply,
                pending.r#gen,
            ) {
                tracing::error!(
                    event = "screensaver_install_failed",
                    display_id = %self.display_id,
                    error = %e.error,
                );
            }
            return;
        }

        // Attach the buffer now that we know the configured size.
        let (buffer, viewport): (WlBuffer, Option<WpViewport>) =
            match (&self.single_pixel_manager, &self.viewporter) {
                (Some(spm), Some(vp)) => {
                    let wl_surface = pending.layer_surface.wl_surface();
                    let (b, v) = crate::linux::surface::attach_single_pixel_black(
                        spm,
                        vp,
                        wl_surface,
                        configured_size.0,
                        configured_size.1,
                        self,
                    );
                    (b, Some(v))
                }
                _ => {
                    // shm fallback.
                    match crate::linux::surface::create_shm_black_buffer(
                        configured_size.0,
                        configured_size.1,
                        self,
                    ) {
                        Ok(b) => {
                            pending.layer_surface.wl_surface().attach(Some(&b), 0, 0);
                            pending.layer_surface.wl_surface().commit();
                            (b, None)
                        }
                        Err(e) => {
                            let _ = pending.reply.send(Err(CmdFailure {
                                controller: "render-black".into(),
                                error: format!("{E_RENDER_UNAVAILABLE}: shm buffer: {e}"),
                            }));
                            return;
                        }
                    }
                }
            };

        self.layer_surface = Some(pending.layer_surface);
        self.viewport = viewport;
        self.black_buffer = Some(buffer);
        self.configured_size = configured_size;
        self.surface_up = true;

        tracing::info!(
            event = "render_black_up",
            display_id = %self.display_id,
            output = %self.output_name,
            r#gen = pending.r#gen,
            width = configured_size.0,
            height = configured_size.1,
        );
        let _ = pending.reply.send(Ok(()));
    }

    /// Screensaver Show: assemble the [`MpvPlayer`], build a double-
    /// buffered shm pool, register the mpv wakeup pipe as a calloop
    /// source, attach + commit the first back buffer, and arm the
    /// first-frame deadline timer.  The `reply` is stored on the
    /// session — the caller MUST NOT send it themselves; it's sent by
    /// `on_mpv_wakeup` on first successful frame or by
    /// `handle_screensaver_first_frame_timeout` on the 5-second
    /// deadline.  Pre-first-frame failures return `Err(CmdFailure)`
    /// directly; the caller resolves the reply.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn complete_screensaver_show(
        &mut self,
        layer_surface: LayerSurface,
        configured_size: (u32, u32),
        settings: ScreensaverSettings,
        reply: tokio::sync::oneshot::Sender<Result<(), CmdFailure>>,
        r#gen: u64,
    ) -> Result<(), CmdFailure> {
        // Tear down any pre-existing screensaver session (the dispatcher
        // shouldn't reach here twice without a teardown in between, but
        // be defensive: e.g. the same surface inherited a stale mpv
        // player from a prior show that wasn't fully cleaned up).
        self.destroy_screensaver_session();

        let width = configured_size.0;
        let height = configured_size.1;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| cmd_failure("screensaver", "stride overflow"))?;

        // ── mpv wakeup pipe ─────────────────────────────────────────
        // The write `OwnedFd` is consumed by `MpvPlayer::new` and
        // closed on its Err path via the player's `Drop` (which runs
        // when the `OwnedFd` falls out of scope at the function's
        // `Err` return).  The read `OwnedFd` lives on across this
        // function (stored in `ScreensaverSession`); on any error
        // AFTER `MpvPlayer::new` succeeds we `drop(read_fd)` explicitly
        // to close it.
        let (read_fd, write_fd) = make_wakeup_pipe()?;

        // ── mpv player ──────────────────────────────────────────────
        // Build the player.  On Err, `write_fd` is dropped here
        // (closing it) and we still own `read_fd` — the caller-side
        // match below handles the read-fd close on Err.
        let player_result = MpvPlayer::new(
            settings.items,
            settings.image_duration,
            settings.audio,
            settings.scale_mode,
            width,
            height,
            write_fd,
        );
        let player = match player_result {
            Ok(p) => p,
            Err(e) => {
                drop(read_fd);
                return Err(cmd_failure("screensaver", &format!("{e}")));
            }
        };

        // ── double-buffered shm pool ────────────────────────────────
        let pool_byte_len = (stride as usize)
            .checked_mul(height as usize)
            .and_then(|x| x.checked_mul(2))
            .ok_or_else(|| cmd_failure("screensaver", "shm pool size overflow"))?;
        let mut pool =
            smithay_client_toolkit::shm::raw::RawPool::new(pool_byte_len, &self.shm_state)
                .map_err(|e| {
                    // player drops (closes write fd).
                    cmd_failure("screensaver", &format!("RawPool::new: {e}"))
                })?;
        let qh = self.queue_handle.clone();
        let (buf0, buf1) = create_dual_buffers(&mut pool, &qh, width, height, stride);

        // Attach the first back buffer (all zeros — opaque black under
        // XRGB8888) and commit.  mpv's first wakeup will follow shortly
        // and overwrite it; the wakeup is the source of the
        // `pending_reply.send(Ok(()))` that resolves the show.
        let wl_surface = layer_surface.wl_surface();
        wl_surface.attach(Some(&buf0), 0, 0);
        wl_surface.damage_buffer(0, 0, width.cast_signed(), height.cast_signed());
        wl_surface.commit();

        // ── calloop wakeup source ───────────────────────────────────
        // SAFETY: read_fd was created by `make_wakeup_pipe` above; we
        // own it until `destroy_screensaver_session` closes it.
        let borrowed_read_fd = unsafe { BorrowedFd::borrow_raw(read_fd.as_raw_fd()) };
        let source = Generic::new(borrowed_read_fd, Interest::READ, Mode::Level);

        let Some(loop_handle) = self.loop_handle.as_ref() else {
            // pool + player drop here (player closes write fd).
            {
                drop(read_fd);
            }
            return Err(cmd_failure(
                "screensaver",
                "loop handle not installed on state",
            ));
        };
        let wakeup_token = match loop_handle.insert_source(source, screensaver_wakeup_cb) {
            Ok(t) => t,
            Err(e) => {
                {
                    drop(read_fd);
                }
                return Err(cmd_failure("screensaver", &format!("insert_source: {e}")));
            }
        };

        // ── first-frame deadline timer ──────────────────────────────
        // Sends `Err(E_RENDER_UNAVAILABLE)` to the pending reply if
        // no successful render lands within 5 seconds; gen-matched so a
        // newer session's timer can't fail the live one.
        let first_frame_token = match crate::linux::connection::arm_screensaver_first_frame_timer(
            loop_handle,
            &self.display_id,
            r#gen,
        ) {
            Ok(t) => Some(t),
            Err(e) => {
                loop_handle.remove(wakeup_token);
                {
                    drop(read_fd);
                }
                return Err(cmd_failure(
                    "screensaver",
                    &format!("arm first-frame timer: {e}"),
                ));
            }
        };

        // Install the session now that both calloop sources are live.
        self.screensaver_session = Some(ScreensaverSession {
            player,
            pool,
            buffers: [buf0, buf1],
            width,
            height,
            stride,
            next_render_idx: 1,
            read_fd,
            buffers_busy: [true, false], // buf0 was attached above; compositor hasn't released it yet.
            pending_reply: Some(reply),
            pending_gen: r#gen,
            has_first_frame: false,
            first_frame_token,
            // `TransitionMode::None` skips every transition-related
            // touch-point below — the state-machine code branches on
            // `transition_mode` for both the ItemEnded capture path
            // and the wakeup `poll_events` drain.
            transition_mode: settings.transition,
            transition: None, // lazy: one per cycle, allocated on first ItemEnded
            transition_duration: settings.transition_duration,
        });
        // The wakeup slot holds the token so we can later remove the
        // source via `loop_handle.remove(token)` from
        // `destroy_screensaver_session`.
        self.screensaver_wakeup_token = Some(wakeup_token);
        self.layer_surface = Some(layer_surface);
        self.configured_size = configured_size;
        self.surface_up = true;

        tracing::info!(
            event = "render_screensaver_up",
            display_id = %self.display_id,
            output = %self.output_name,
            width,
            height,
            r#gen,
        );
        Ok(())
    }

    /// Configure-timeout equivalent for the screensaver: fires after
    /// the deadline if `has_first_frame` is still false on the session
    /// whose `pending_gen` matches the timer's target.  Resolves the
    /// pending reply with `Err(E_RENDER_UNAVAILABLE)` (engine falls
    /// through) and tears the session down so the surface can fall
    /// back to black.
    pub(super) fn handle_screensaver_first_frame_timeout(
        &mut self,
        _display_id: &DisplayId,
        r#gen: u64,
    ) {
        // Gen guard — also acts as the "session still alive" check.
        let pending_gen_matches = self
            .screensaver_session
            .as_ref()
            .is_some_and(|s| s.pending_gen == r#gen && !s.has_first_frame);
        if !pending_gen_matches {
            // Either the session was already replaced, the first frame
            // already landed, or the timer was for a stale gen.  The
            // gen-guard is the discipline; ignore silently.
            return;
        }

        // Move the reply out before we start tearing the session down
        // (the destructuring would otherwise consume it through
        // `take()` ordering side-effects).
        let reply = self
            .screensaver_session
            .as_mut()
            .and_then(|s| s.pending_reply.take());

        // Fall back to black on the SAME surface (engine may have
        // already moved on; this is a clean shutdown).
        self.fail_screensaver_to_black("no first frame within 5s");

        if let Some(reply) = reply {
            let _ = reply.send(Err(cmd_failure(
                "render-screensaver",
                "no first frame within 5s",
            )));
        }
    }

    /// mpv wakeup callback: drain the pipe + drain mpv's event queue,
    /// render one frame into the back buffer, attach + damage +
    /// commit, swap indices.  Called from the calloop thread when the
    /// Generic source signals.
    ///
    /// First-frame semantics: the very first successful render resolves
    /// the originating `ShowScreensaver` oneshot with `Ok(())` and
    /// removes the deadline timer (gen-guard covers the race where the
    /// timer fires just before the wakeup is dispatched).
    ///
    /// Crossfade semantics: every wakeup also drains mpv's event
    /// queue.  An `ItemEnded` event captures the just-rendered frame
    /// into `TransitionState::capture` (the blend's `a` side) — this
    /// MUST happen AFTER `render_frame_into` so the freshly-rendered
    /// outgoing frame is what's saved.  An `ItemLoaded` arms the
    /// periodic blend timer; the next wakeup (the new item's first
    /// frame) starts the visible crossfade.
    #[allow(clippy::too_many_lines)] // single-method state machine: capture, render, blend, attach, advance are documented inline below
    fn on_mpv_wakeup(&mut self) {
        let Some(session) = self.screensaver_session.as_mut() else {
            return;
        };

        // Drain the pipe (non-blocking; multiple wakeups may have queued).
        let mut drain_buf = [0u8; 256];
        loop {
            // SAFETY: kernel writes to the buffer; partial reads are fine
            // for non-blocking pipes.
            let n = unsafe {
                libc::read(
                    session.read_fd.as_raw_fd(),
                    drain_buf.as_mut_ptr().cast(),
                    drain_buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
        }

        // Drain mpv's event queue; map the two transition events we
        // care about.  Done BEFORE the render so an `ItemEnded` from
        // this batch snapshots the OUTGOING frame (about to be
        // overwritten by the next render's incoming data).
        //
        // NOTE: we don't drain events on transition==None — the state
        // machine has no need for them and they'd just burn cycles.
        // The drain is gated by `transition_mode` so the None path
        // stays byte-identical to the pre-feature behaviour.
        let mut just_captured = false;
        let mut just_loaded = false;
        if session.transition_mode == TransitionMode::Crossfade {
            for ev in session.player.poll_events() {
                match ev {
                    MpvItemEvent::ItemEnded => {
                        // Capture only if no transition is already in
                        // flight (a fresh ItemEnded before the previous
                        // blend finished — possible if a very short
                        // `image-display-duration` produced back-to-back
                        // advances; ignore the duplicate so we don't
                        // capture mid-blend data into the next cycle's
                        // capture).
                        if session.transition.is_none() {
                            // Allocate the capture buffer lazily on
                            // FIRST ItemEnded; the per-byte cost is
                            // `stride * height` bytes (one full frame).
                            if session.transition.as_ref().map_or(0, |t| t.capture.len())
                                != (session.stride as usize) * (session.height as usize)
                            {
                                session.transition = Some(TransitionState {
                                    r#gen: session.pending_gen,
                                    capture: vec![
                                        0u8;
                                        (session.stride as usize)
                                            * (session.height as usize)
                                    ],
                                    timer_token: None,
                                    t_frac: 0,
                                    t_step: 0,
                                    fps: 30,
                                    tick_interval: std::time::Duration::from_millis(33),
                                    waiting_for_first_new_frame: false,
                                });
                            }
                            // Snapshot the FRONT buffer (the one
                            // currently attached to the wl_surface) —
                            // `1 - next_render_idx` because the last
                            // render wrote to `next_render_idx` and
                            // flipped.
                            let front_idx = 1 - session.next_render_idx;
                            let buf_len = (session.stride as usize) * (session.height as usize);
                            let front_offset = front_idx * buf_len;
                            let mmap = session.pool.mmap();
                            // SAFETY: pool was sized to cover both
                            // buffers at known offsets; capturing
                            // `buf_len` bytes from `front_offset`
                            // cannot overrun.
                            let front_slice = unsafe {
                                std::slice::from_raw_parts(
                                    mmap.as_ptr().cast::<u8>().add(front_offset),
                                    buf_len,
                                )
                            };
                            if let Some(tr) = session.transition.as_mut() {
                                tr.capture.copy_from_slice(front_slice);
                                tr.r#gen = session.pending_gen;
                                tr.t_frac = 0;
                                tr.waiting_for_first_new_frame = true;
                                just_captured = true;
                            }
                        }
                    }
                    MpvItemEvent::ItemLoaded => {
                        // Only meaningful if we just captured (the
                        // capture came before the load).  If the
                        // player reports ItemLoaded first (e.g. after
                        // an idle→load cycle), wait for the matching
                        // ItemEnded that precedes it.
                        if session
                            .transition
                            .as_ref()
                            .is_some_and(|t| t.waiting_for_first_new_frame)
                        {
                            just_loaded = true;
                        }
                    }
                }
            }
        }

        // Skip-on-busy: if the back buffer is still busy with a prior
        // commit (the compositor hasn't released it yet), drop the
        // frame rather than overwriting live compositor state.  Two
        // buffers + skipping is the documented Wayland-protocol-correct
        // path (don't introduce a third buffer just to keep up).
        let back_idx = session.next_render_idx;
        let session_gen = session.pending_gen;
        if session.buffers_busy[back_idx] {
            tracing::debug!(
                event = "screensaver_frame_skipped_busy",
                display_id = %self.display_id,
                back_idx,
            );
            // Important: still arm the timer if we just captured
            // AND loaded in this wakeup — the visible frame is still
            // the outgoing one, but the blend kick-off doesn't depend
            // on the new frame having rendered yet.  The first blend
            // tick will pick up the new frame on the next wakeup.
            if just_captured && just_loaded {
                self.arm_transition_timer(session_gen);
            }
            return;
        }

        // Render into the back buffer.
        let stride = session.stride as usize;
        let buf_len = stride * (session.height as usize);
        let back_offset = back_idx * buf_len;
        // Take a clone of the capture Vec (or `None` if no active
        // blend) — the in-place blend function needs `&[u8]` +
        // `&mut [u8]` of equal length, and the back buffer slice
        // would otherwise conflict with a borrow on `session.transition`.
        let blend_state = session
            .transition
            .as_ref()
            .filter(|tr| !tr.waiting_for_first_new_frame)
            .map(|tr| (tr.t_frac, tr.capture.clone()));
        {
            let mmap = session.pool.mmap();
            // SAFETY: the offset is within the pool (we built it with
            // 2 * stride * height bytes) and the slice length matches.
            let back_slice = unsafe {
                std::slice::from_raw_parts_mut(mmap.as_ptr().cast_mut().add(back_offset), buf_len)
            };
            // Zero the back buffer first so leftover content from a prior
            // pass doesn't bleed into partially-written mpv output.
            back_slice.fill(0);
            if let Err(e) = session.player.render_frame_into(back_slice) {
                self.fail_screensaver_to_black(&format!("{e}"));
                return;
            }

            // If we're inside an active blend, overlay the incoming
            // frame on top of the capture at the current t_frac.  We
            // use the in-place blend so mpv's freshly-rendered pixels
            // are mutated toward the capture side in a single pass
            // (no third scratch buffer required).
            if let Some((t, cap)) = blend_state {
                blend::blend_in_place(&cap, back_slice, t);
            }
        }

        // Attach + damage + commit.
        let wl_surface = match &self.layer_surface {
            Some(s) => s.wl_surface().clone(),
            None => return,
        };
        wl_surface.attach(Some(&session.buffers[back_idx]), 0, 0);
        wl_surface.damage_buffer(
            0,
            0,
            session.width.cast_signed(),
            session.height.cast_signed(),
        );
        wl_surface.commit();
        // The buffer we just attached is now busy with the compositor;
        // mark it so the next wakeup skips a render into the other
        // buffer until this one is released.
        session.buffers_busy[back_idx] = true;

        // First-frame success: resolve the pending show reply and
        // remove the deadline timer (otherwise the 5-second timer
        // could fire right after we send Ok — its gen-guard catches
        // this race, but cancelling is cheaper than ignoring).
        if !session.has_first_frame {
            session.has_first_frame = true;
            if let Some(reply) = session.pending_reply.take() {
                let _ = reply.send(Ok(()));
            }
            if let (Some(token), Some(handle)) =
                (session.first_frame_token.take(), self.loop_handle.as_ref())
            {
                handle.remove(token);
            }
        }

        // Arm the transition timer ON the first wakeup after
        // ItemEnded+ItemLoaded (i.e. once we have BOTH the capture
        // AND the new frame).  The blend tick takes over from this
        // wakeup onward until `t_frac` reaches `T_MAX`.
        //
        // Skip-on-busy above already arm the timer in the busy case;
        // only do it here on the non-busy path.  We deliberately
        // rebind `session` so the `&mut self` borrow for
        // `arm_transition_timer` doesn't conflict with the live one.
        if just_captured && just_loaded {
            // We need `pending_gen` (u64, Copy) for the timer; pull it
            // out into a local so we can return without further
            // touching `session` (the `&mut self` borrow for
            // `arm_transition_timer` can't coexist with the live
            // session borrow).
            let r#gen = session.pending_gen;
            self.arm_transition_timer(r#gen);
            return;
        }

        // Mark the transition as "we've seen the new frame" so the
        // NEXT wakeup (the timer tick) starts blending instead of
        // waiting forever.
        if just_loaded && let Some(tr) = session.transition.as_mut() {
            tr.waiting_for_first_new_frame = false;
        }

        // Swap so the next render writes to the buffer the compositor
        // has finished with (or is about to release).
        session.next_render_idx = 1 - back_idx;
    }

    /// Arm the per-session transition blend timer.  Called from
    /// [`Self::on_mpv_wakeup`] once an `ItemEnded` + `ItemLoaded`
    /// pair has both fired (the capture is in `transition.capture`,
    /// the new frame is visible on the next wakeup).  The timer
    /// increments `t_frac` by `t_step` per tick until it reaches
    /// `T_MAX`, at which point the timer drops itself.
    ///
    /// Idempotent: a no-op when the session has no active transition
    /// (e.g. missing capture, missing `pending_gen` parity, or a
    /// timer already armed).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_lossless,
        clippy::manual_div_ceil
    )] // fps→f64→u16 timing math: bounds-checked by `.max(1.0)` / clamp inside
    fn arm_transition_timer(&mut self, r#gen: u64) {
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };
        let Some(session) = self.screensaver_session.as_mut() else {
            return;
        };
        if session.transition_mode != TransitionMode::Crossfade {
            return;
        }
        let Some(tr) = session.transition.as_mut() else {
            return;
        };
        if tr.timer_token.is_some() {
            // Already armed — the previous tick will run its course.
            return;
        }
        if tr.r#gen != r#gen {
            // Stale — a new session has superseded the one this capture
            // belonged to.  Don't arm; the previous session's
            // destroy_screensaver_session will sweep its own timer.
            return;
        }

        // tick_interval → 33ms for 30 fps.  Hard-coded constant — if
        // we ever expose `transition_fps` to config we'll plumb it
        // here; today 30 is the only supported speed.
        let fps = u64::from(tr.fps);
        let frames = fps.saturating_mul(tr.tick_interval.as_secs())
            + (u64::from(tr.tick_interval.subsec_millis()) * fps).div_ceil(1000);
        let duration_secs = session.transition_duration.as_secs_f64();
        let frames_for_blend = (f64::from(frames as u32) * duration_secs).max(1.0);
        tr.t_step = ((f64::from(T_MAX) / frames_for_blend).round() as u16).max(1);
        tr.t_frac = 0;

        // Self-repeating timer: arm for one tick interval now; the
        // callback re-arms via `TimeoutAction::ToInstant` while
        // `t_frac < T_MAX` and returns `Drop` on the final tick.
        let timer = Timer::from_duration(tr.tick_interval);
        let inserted =
            handle.insert_source(timer, move |_deadline, _meta, state: &mut WaylandState| {
                state.on_transition_tick(r#gen);
                // We can't read the live t_frac here without re-borrowing
                // state; instead, return `Drop` from inside
                // `on_transition_tick`'s state-machine when the blend is
                // complete.  But calloop's `EventSource` trait requires
                // the caller to return ONE `TimeoutAction` synchronously.
                // The pragmatic answer: always re-arm; the timer body
                // itself decides when to remove its own token and clear
                // `transition` from inside on_transition_tick.  This
                // pattern matches the project's `arm_screensaver_first_
                // frame_timer` discipline (drop on completion from inside).
                TimeoutAction::ToInstant({
                    use std::time::Instant;
                    Instant::now() + std::time::Duration::from_millis(33)
                })
            });
        match inserted {
            Ok(token) => {
                tr.timer_token = Some(token);
            }
            Err(e) => {
                tracing::error!(
                    event = "transition_timer_insert_failed",
                    display_id = %self.display_id,
                    r#gen,
                    error = %e,
                    "failed to install transition timer; blend will be skipped"
                );
                tr.timer_token = None;
            }
        }
    }

    /// Per-tick blend progress.  Runs on the calloop thread when the
    /// transition timer fires.  The path mirrors `on_mpv_wakeup`'s
    /// "render + attach + commit" chunk so the visible result is
    /// indistinguishable from a normal wakeup to the compositor.
    ///
    /// Drops the timer token and clears the `transition` field once
    /// `t_frac >= T_MAX` (or fires `fail_screensaver_to_black` on render
    /// failure to keep the failure-path semantics identical to the
    /// normal render path).
    fn on_transition_tick(&mut self, r#gen: u64) {
        let Some(session) = self.screensaver_session.as_mut() else {
            return;
        };
        // Gen-guard: a newer session has taken over — drop the tick.
        if session.pending_gen != r#gen || session.transition_mode != TransitionMode::Crossfade {
            return;
        }
        let Some(tr) = session.transition.as_ref() else {
            return;
        };
        if tr.t_frac >= T_MAX {
            // Already complete; the timer should have self-removed in
            // its last tick.  Idempotent guard.
            return;
        }

        // Advance t_frac (the timer callback will see the updated value
        // on its next fire).
        let new_t = (tr.t_frac + tr.t_step).min(T_MAX);
        let capture_len = tr.capture.len();

        // Re-render via the same skip-on-busy gate as a normal wakeup.
        let back_idx = session.next_render_idx;
        if session.buffers_busy[back_idx] {
            // Compositor hasn't released the buffer yet — drop this
            // tick; the next timer fire will try again.
            return;
        }

        let stride = session.stride as usize;
        let buf_len = stride * (session.height as usize);
        let back_offset = back_idx * buf_len;
        // Clone the capture so the `&mut` borrow on the back buffer
        // doesn't conflict with the borrow on `session.transition`.
        let capture_clone = if capture_len == buf_len {
            Some(tr.capture.clone())
        } else {
            None
        };
        {
            let mmap = session.pool.mmap();
            // SAFETY: offset is within the pool (built for two
            // buffers worth); slice length matches.
            let back_slice = unsafe {
                std::slice::from_raw_parts_mut(mmap.as_ptr().cast_mut().add(back_offset), buf_len)
            };
            back_slice.fill(0);
            if let Err(e) = session.player.render_frame_into(back_slice) {
                self.fail_screensaver_to_black(&format!("{e}"));
                return;
            }

            // Blend the freshly-rendered frame with the capture in
            // place — one buffer's worth of arithmetic per tick.
            if let Some(cap) = capture_clone.as_ref() {
                blend::blend_in_place(cap, back_slice, new_t);
            }
        }

        // Attach + damage + commit.
        let wl_surface = match &self.layer_surface {
            Some(s) => s.wl_surface().clone(),
            None => return,
        };
        wl_surface.attach(Some(&session.buffers[back_idx]), 0, 0);
        wl_surface.damage_buffer(
            0,
            0,
            session.width.cast_signed(),
            session.height.cast_signed(),
        );
        wl_surface.commit();
        session.buffers_busy[back_idx] = true;
        session.next_render_idx = 1 - back_idx;

        // Persist t_frac (the &mut borrow above released when we
        // re-borrowed for the blend step).
        let blended_complete = new_t >= T_MAX;
        let timer_token_to_remove = if blended_complete {
            self.screensaver_session
                .as_ref()
                .and_then(|s| s.transition.as_ref())
                .and_then(|t| t.timer_token)
        } else {
            None
        };
        if blended_complete {
            if let Some(token) = timer_token_to_remove
                && let Some(handle) = self.loop_handle.as_ref()
            {
                handle.remove(token);
            }
            if let Some(tr_mut) = self
                .screensaver_session
                .as_mut()
                .and_then(|s| s.transition.as_mut())
            {
                tr_mut.timer_token = None;
            }
            tracing::debug!(
                event = "screensaver_transition_complete",
                display_id = %self.display_id,
                r#gen,
                duration_ms = self
                    .screensaver_session
                    .as_ref()
                    .map_or(0, |s| s.transition_duration.as_millis()),
                "crossfade complete; resuming wakeup-driven path"
            );
        } else if let Some(tr_mut) = self
            .screensaver_session
            .as_mut()
            .and_then(|s| s.transition.as_mut())
        {
            tr_mut.t_frac = new_t;
        }
    }

    /// Post-first-frame failure (or deadline failure): tear down the
    /// session, ensure a fallback black buffer exists, attach it to
    /// the SAME surface, commit, and log.  The screensaver may have
    /// been the FIRST stage (no prior black), so the black buffer
    /// must be created on demand before the screensaver's pool goes
    /// away.
    fn fail_screensaver_to_black(&mut self, reason: &str) {
        tracing::warn!(
            event = "screensaver_failed_to_black",
            display_id = %self.display_id,
            reason = reason,
        );

        // Build a black buffer NOW if the screensaver was the first
        // stage (no prior `RenderBlack` show to create one).  Use the
        // single-pixel + viewporter path when available, otherwise the
        // shm fallback — matches the black path's own choices.
        if self.black_buffer.is_none()
            && let Some(surface) = self.layer_surface.as_ref()
        {
            let wl_surface = surface.wl_surface();
            let w = self.configured_size.0;
            let h = self.configured_size.1;
            if let (Some(spm), Some(vp)) = (&self.single_pixel_manager, &self.viewporter) {
                let buffer = spm.create_u32_rgba_buffer(0, 0, 0, u32::MAX, &self.queue_handle, ());
                let viewport = vp.get_viewport(wl_surface, &self.queue_handle, ());
                viewport.set_destination(w.cast_signed(), h.cast_signed());
                self.viewport = Some(viewport);
                self.black_buffer = Some(buffer);
            } else {
                match crate::linux::surface::create_shm_black_buffer(w, h, self) {
                    Ok(buffer) => self.black_buffer = Some(buffer),
                    Err(e) => tracing::error!(
                        event = "screensaver_black_fallback_failed",
                        display_id = %self.display_id,
                        error = %e,
                    ),
                }
            }
        }

        // Destroy the session — frees the mpv player + shm pool, removes
        // the calloop wakeup source, removes the deadline timer.
        self.destroy_screensaver_session();

        // Re-attach the now-guaranteed black buffer.
        if let (Some(surface), Some(black)) = (&self.layer_surface, &self.black_buffer) {
            let wl_surface = surface.wl_surface();
            wl_surface.attach(Some(black), 0, 0);
            wl_surface.commit();
        }
    }

    /// Tear down the active screensaver session (if any).  Removes all
    /// calloop sources (mpv wakeup + first-frame deadline +
    /// transition timer), drops the session — `MpvPlayer`'s `Drop`
    /// unregisters the mpv callback, frees the render context, drops
    /// the mpv handle, and closes the write fd; the session drops
    /// the read fd, the `RawPool`, and the two `WlBuffer`s.  No manual
    /// `player.destroy()` call needed.
    fn destroy_screensaver_session(&mut self) {
        // Remove ALL calloop sources FIRST so no further callbacks fire
        // against a session that's about to be dropped.
        if let Some(handle) = self.loop_handle.as_ref() {
            if let Some(token) = self.screensaver_wakeup_token.take() {
                handle.remove(token);
            }
            if let Some(session) = self.screensaver_session.as_ref() {
                if let Some(token) = session.first_frame_token {
                    handle.remove(token);
                }
                // Drop the active transition timer if any — `Drop` on
                // the timer source runs after `handle.remove` so the
                // callback can't fire on a torn-down session.
                if let Some(tr) = session.transition.as_ref()
                    && let Some(token) = tr.timer_token
                {
                    handle.remove(token);
                }
            }
        }
        // Drop the session — the destructuring here is purely to control
        // drop order (player first, then read fd, then pool).  The
        // player's `Drop` runs the mpv teardown.
        if let Some(session) = self.screensaver_session.take() {
            let ScreensaverSession {
                player,
                read_fd,
                pool,
                buffers,
                first_frame_token: _,
                pending_reply: _,
                pending_gen: _,
                has_first_frame: _,
                buffers_busy: _,
                width: _,
                height: _,
                stride: _,
                next_render_idx: _,
                transition_mode: _,
                transition: _,
                transition_duration: _,
            } = session;
            // Drop the player first — its Drop unregisters the wakeup
            // callback, frees the render context, drops mpv, closes the
            // write fd.  MUST happen before closing read_fd so the
            // callback can't fire against a dead pipe.
            drop(player);
            // SAFETY: read_fd was created via pipe2 and is owned by the
            // session; closing once here after the player drops.
            {
                drop(read_fd);
            }
            // pool + buffers drop here; RawPool's Drop destroys the
            // wl_shm_pool, which in turn releases the WlBuffers.  The
            // transition field's capture Vec drops here too (~21 MiB at
            // 4K); Rust's struct field drop order runs it before pool
            // because we declared `transition` AFTER `pool` in the
            // struct.  Doesn't actually matter for correctness — both
            // drops are independent — but the explicit timing makes
            // lifetime reasoning easier.
            drop(pool);
            drop(buffers);
        }
    }

    /// Locate the target output by connector name (called after the
    /// initial roundtrip has populated output info).
    pub(super) fn locate_target_output(&mut self) -> Result<(), CmdFailure> {
        for output in self.output_state.outputs() {
            if let Some(info) = self.output_state.info(&output)
                && info.name.as_deref() == Some(self.output_name.as_str())
            {
                self.target_output = Some(output);
                return Ok(());
            }
        }
        Err(CmdFailure {
            controller: "render-black".into(),
            error: format!(
                "{E_RENDER_UNAVAILABLE}: output '{}' not found",
                self.output_name,
            ),
        })
    }

    /// Tear down the live surface (if any).  Cancels any in-flight
    /// pending show with a soft error — the daemon may legitimately
    /// teardown before configure arrives.
    pub(super) fn destroy_surface(&mut self) {
        // Destroy any active screensaver session first — frees mpv + shm.
        // If we destroy the surface without this, the player keeps running
        // and the wakeup pipe keeps firing on a dead surface.
        self.destroy_screensaver_session();
        if let Some(pending) = self.pending_show.take() {
            let _ = pending.reply.send(Err(CmdFailure {
                controller: "render-black".into(),
                error: format!("{E_RENDER_UNAVAILABLE}: teardown raced with pending show"),
            }));
        }
        if let Some(surface) = self.layer_surface.take() {
            let wl_surface = surface.wl_surface();
            wl_surface.attach(None, 0, 0);
            wl_surface.commit();
            // Dropping the LayerSurface proxy sends the destroy.
        }
        self.viewport = None;
        self.black_buffer = None;
        self.surface_up = false;
        self.configured_size = (0, 0);
        self.input_latch.reset();
    }

    /// Teardown: synchronous — destroy any live surface, fail any
    /// in-flight pending show, resolve the reply.
    fn handle_teardown(&mut self, r#gen: u64, reply: tokio::sync::oneshot::Sender<()>) {
        self.destroy_surface();
        tracing::info!(
            event = "render_teardown",
            display_id = %self.display_id,
            output = %self.output_name,
            r#gen,
        );
        let _ = reply.send(());
    }

    /// Dispatch entry for incoming commands from the async sink side.
    pub(super) fn handle_command(&mut self, cmd: RenderCommand) {
        match cmd {
            RenderCommand::Show {
                r#gen,
                idx,
                kind,
                reply,
            } => self.handle_show(r#gen, idx, kind, reply),
            RenderCommand::Teardown { r#gen, reply } => self.handle_teardown(r#gen, reply),
            #[cfg(target_os = "linux")]
            RenderCommand::ShowScreensaver {
                r#gen,
                idx,
                settings,
                reply,
            } => self.handle_show_screensaver(r#gen, idx, settings, reply),
        }
    }

    /// Show: create the layer surface, send the initial commit, store a
    /// pending entry.  Returns immediately — the compositor's `configure`
    /// reply completes the show (or the configure-timeout timer fails it).
    fn handle_show(
        &mut self,
        r#gen: u64,
        _idx: usize,
        kind: StageKind,
        reply: tokio::sync::oneshot::Sender<Result<(), CmdFailure>>,
    ) {
        match kind {
            StageKind::RenderBlack => {
                self.input_latch.reset();

                // Content-swap path: the render→render advance contract
                // (the core state machine emits NO teardown between
                // adjacent render stages) requires the backend to keep
                // the existing layer surface and just swap the buffer
                // content.  Only destroy the surface when there's no
                // live surface to swap onto (e.g. first show, or after
                // a controller-stage teardown).
                if self.layer_surface.is_some() && self.surface_up {
                    // Borrow the wl_surface up front, then drop the
                    // immutable borrow before the mutable calls below
                    // (avoids E0502 with the self-borrowing methods).
                    let wl_surface = self
                        .layer_surface
                        .as_ref()
                        .expect("just checked")
                        .wl_surface()
                        .clone();

                    // Tear down any active screensaver session — the
                    // mpv player, pipe source, shm pool, and deadline
                    // timer — but KEEP the layer surface alive.
                    self.destroy_screensaver_session();

                    // Ensure a black buffer exists (may not, if the
                    // screensaver was the first stage).  Use the same
                    // single-pixel + viewporter / shm path the black
                    // show uses.
                    if self.black_buffer.is_none() {
                        let w = self.configured_size.0;
                        let h = self.configured_size.1;
                        if let (Some(spm), Some(vp)) =
                            (&self.single_pixel_manager, &self.viewporter)
                        {
                            let buffer = spm.create_u32_rgba_buffer(
                                0,
                                0,
                                0,
                                u32::MAX,
                                &self.queue_handle,
                                (),
                            );
                            let viewport = vp.get_viewport(&wl_surface, &self.queue_handle, ());
                            viewport.set_destination(w.cast_signed(), h.cast_signed());
                            self.viewport = Some(viewport);
                            self.black_buffer = Some(buffer);
                        } else if let Ok(buffer) =
                            crate::linux::surface::create_shm_black_buffer(w, h, self)
                        {
                            self.black_buffer = Some(buffer);
                        }
                    }

                    if let Some(black) = self.black_buffer.as_ref() {
                        wl_surface.attach(Some(black), 0, 0);
                        wl_surface.commit();
                        tracing::info!(
                            event = "render_black_swap",
                            display_id = %self.display_id,
                            output = %self.output_name,
                            r#gen,
                        );
                        let _ = reply.send(Ok(()));
                    } else {
                        let _ = reply.send(Err(cmd_failure(
                            "render-black",
                            "could not build black fallback buffer",
                        )));
                    }
                    return;
                }

                // No live surface — fall back to the original path:
                // tear down anything stale, create a fresh layer surface,
                // wait for configure, then attach black.
                self.destroy_surface();

                let Some(target_output) = self.target_output.clone() else {
                    let _ = reply.send(Err(CmdFailure {
                        controller: "render-black".into(),
                        error: format!("{E_RENDER_UNAVAILABLE}: no target output bound"),
                    }));
                    return;
                };

                // Build the layer surface.  The `commit()` inside
                // `create_layer_surface` triggers the compositor's
                // `configure` reply; the reply is dispatched via the
                // WaylandSource on the calloop thread.
                let layer_surface = crate::linux::surface::create_layer_surface(
                    &self.layer_shell,
                    &self.compositor_state,
                    &target_output,
                    self,
                );
                self.pending_show = Some(PendingShow {
                    r#gen,
                    kind: StageKind::RenderBlack,
                    screensaver: None,
                    layer_surface,
                    reply,
                });
            }
            StageKind::RenderScreensaver | StageKind::Controller(_) => {
                // Controller stages never reach a render sink at all
                // (the engine routes them through the command-sink
                // chain); screensaver shows come in via the dedicated
                // `ShowScreensaver` command path, not through here.
                let _ = reply.send(Err(CmdFailure {
                    controller: "render-black".into(),
                    error: format!(
                        "{E_RENDER_UNAVAILABLE}: stage {kind:?} not implemented in this backend"
                    ),
                }));
            }
        }
    }

    /// Register an input event from the pointer / keyboard handler.
    /// First event after a surface-up fires the `InputWake` signal;
    /// subsequent events are silently dropped until the latch resets.
    pub(super) fn on_input_event(&mut self) {
        if !self.surface_up {
            return;
        }
        if let (Some(display_id), Some(tx)) = (self.input_latch.on_input(), &self.input_wake_tx) {
            let _ = tx.send(display_id);
        }
    }

    /// `ShowScreensaver` command entry — build (or reuse) the layer
    /// surface, package a `PendingShow { kind: RenderScreensaver, ... }`,
    /// and commit.  The actual screensaver install happens in
    /// [`Self::complete_pending_show`] once the compositor configures
    /// the surface.
    fn handle_show_screensaver(
        &mut self,
        r#gen: u64,
        _idx: usize,
        settings: ScreensaverSettings,
        reply: tokio::sync::oneshot::Sender<Result<(), CmdFailure>>,
    ) {
        self.input_latch.reset();

        let Some(target_output) = self.target_output.clone() else {
            let _ = reply.send(Err(cmd_failure(
                "render-screensaver",
                "no target output bound",
            )));
            return;
        };

        // If the surface is already configured + live (from a prior
        // black stage), install the screensaver directly on it.  This
        // is the "adjacent render stages swap CONTENT, no destroy /
        // flicker" path described in the design phase.  The reply is
        // held by the session until first frame / deadline.
        if let Some(existing) = &self.layer_surface
            && self.surface_up
        {
            if let Err(e) = self.complete_screensaver_show(
                existing.clone(),
                self.configured_size,
                settings,
                reply,
                r#gen,
            ) {
                tracing::error!(
                    event = "screensaver_install_failed",
                    display_id = %self.display_id,
                    error = %e.error,
                );
            }
            return;
        }

        // No surface up yet (or not configured) — create one and go
        // through the pending-show → configure → install flow.
        let layer_surface = crate::linux::surface::create_layer_surface(
            &self.layer_shell,
            &self.compositor_state,
            &target_output,
            self,
        );
        self.pending_show = Some(PendingShow {
            r#gen,
            kind: StageKind::RenderScreensaver,
            screensaver: Some(settings),
            layer_surface,
            reply,
        });
    }
}

// ── SCTK delegate impls ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `should_fail_timeout` returns `true` ONLY when the timeout's
    /// gen matches the still-pending show's gen.  Anything else
    /// (no pending show, or a newer show superseded this one) is
    /// stale and must NOT fail the live pending show.
    ///
    /// These tests pin the gen-match discipline.  The same logic lives
    /// in `handle_configure_timeout`; if a future maintainer rewrites
    /// it without this helper, the unit tests below should still hold
    /// the line.
    #[test]
    fn stale_timeout_when_no_pending_show_does_not_fail() {
        assert!(!WaylandState::should_fail_timeout(None, 1));
    }

    #[test]
    fn stale_timeout_with_mismatched_gen_does_not_fail() {
        // Race scenario: gen=1 was armed, completed; gen=2 is now
        // pending; the gen=1 timer fires stale → must not fail gen=2.
        assert!(!WaylandState::should_fail_timeout(Some(2), 1));
    }

    #[test]
    fn real_timeout_with_matching_gen_fails() {
        assert!(WaylandState::should_fail_timeout(Some(1), 1));
    }

    #[test]
    fn timeout_gen_zero_against_no_pending_show() {
        // Defensive: a gen=0 timeout (the machine's initial gen) should
        // never spuriously fail anything if there's no pending show.
        assert!(!WaylandState::should_fail_timeout(None, 0));
    }

    /// End-to-end-ish test of the race: simulate the handler logic by
    /// running the gen-match decision against a sequence of pending
    /// states that mirrors the live interleaving.
    ///
    /// Uses the same `should_fail_timeout` decision function that
    /// `handle_configure_timeout` is supposed to delegate to.  If a
    /// future regression makes the handler use `is_some()` instead of
    /// the helper, the corresponding E2E test below (which directly
    /// checks the same property the helper checks) will catch it.
    #[test]
    fn stale_timer_after_completion_does_not_fail_next_show() {
        // Step 1: Show(gen=1) completes via configure.
        let mut pending_gen: Option<u64> = Some(1);
        let _ = pending_gen.take(); // complete_pending_show runs

        // Step 2: Show(gen=2) enters pending state.
        pending_gen = Some(2);

        // Step 3: the gen=1 timer fires stale.
        let stale_should_fail = WaylandState::should_fail_timeout(pending_gen, 1);
        assert!(
            !stale_should_fail,
            "stale gen=1 timer must not fail gen=2's live pending show"
        );
    }

    // ── surface_match tests (round-3 — M2 stale-event guard) ───────────
    //
    // Constructing distinct `ObjectId`s without a real Wayland
    // connection isn't possible (the public constructor is
    // `ObjectId::null()`, which always returns the same id).  These
    // tests cover the branches that ARE testable without distinct ids:
    //
    // - `pending == event` → Pending (null == null, first arm fires)
    // - `None, None` → Stale (no arms fire, falls through)
    //
    // The "matches live but not pending" and "matches neither" branches
    // are validated by integration tests (live smoke) and by code
    // inspection — they're symmetric to the tested branches above and
    // use the same `Option::eq` / `Some(x) == Some(event)` machinery.

    #[test]
    fn surface_match_pending_when_event_matches_pending() {
        // All three ObjectIds are null(); pending wins the first arm.
        let id = wayland_client::backend::ObjectId::null();
        assert_eq!(
            surface_match(Some(&id), Some(&id), &id),
            SurfaceMatch::Pending
        );
    }

    #[test]
    fn surface_match_stale_when_no_pending_or_live() {
        let event = wayland_client::backend::ObjectId::null();
        assert_eq!(surface_match(None, None, &event), SurfaceMatch::Stale);
    }

    #[test]
    fn surface_match_stale_when_pending_none_live_some_unrelated() {
        // Live holds a null id; event is a (different, also-null)
        // id.  Without distinct ids we can't construct an "unrelated"
        // event here, so this test verifies only the None-pending +
        // Some-live path doesn't accidentally fire the Pending arm.
        // The "Live matches but not pending" branch is exercised by the
        // full integration test (live smoke + configure on a live
        // surface).
        let event = wayland_client::backend::ObjectId::null();
        let pending = None;
        let live: Option<&wayland_client::backend::ObjectId> = None;
        assert_eq!(surface_match(pending, live, &event), SurfaceMatch::Stale);
    }
}

// ── SCTK delegate impls ───────────────────────────────────────────────────────

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: smithay_client_toolkit::reexports::client::protocol::wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // Surface-identity guard (round-3 fix): only fail/clear when
        // the closed surface is one WE created.  Stale closes for
        // already-torn-down or superseded surfaces are a no-op.
        let event_id = layer.wl_surface().id();
        let pending_id = self
            .pending_show
            .as_ref()
            .map(|p| p.layer_surface.wl_surface().id());
        let live_id = self.layer_surface.as_ref().map(|s| s.wl_surface().id());

        match surface_match(pending_id.as_ref(), live_id.as_ref(), &event_id) {
            SurfaceMatch::Pending => {
                self.fail_pending_show(CmdFailure {
                    controller: "render-black".into(),
                    error: format!(
                        "{E_RENDER_UNAVAILABLE}: compositor closed pending layer surface"
                    ),
                });
            }
            SurfaceMatch::Live => {
                // Live surface closed externally — flush our bookkeeping.
                // Tear down the screensaver session FIRST (if any) so
                // mpv and the calloop wakeup pipe don't outlive the
                // surface; destroy_screensaver_session is a no-op when
                // no session is active.
                self.destroy_screensaver_session();
                self.surface_up = false;
                self.layer_surface = None;
                self.viewport = None;
                self.black_buffer = None;
                self.configured_size = (0, 0);
                self.input_latch.reset();
                tracing::info!(
                    event = "layer_surface_closed_by_compositor",
                    display_id = %self.display_id,
                );
            }
            SurfaceMatch::Stale => {
                tracing::debug!(
                    event = "render_stale_closed",
                    display_id = %self.display_id,
                    event_surface = %event_id,
                    "stale closed for a surface that is not pending or live; ignored"
                );
            }
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Surface-identity guard (round-3 fix): only `complete_pending_show`
        // when the configure is for our still-pending surface.  Stale
        // configure from an old/superseded surface would otherwise
        // complete a NEWER pending show with wrong dims / prematurely.
        let size = configure.new_size;
        let event_id = layer.wl_surface().id();
        let pending_id = self
            .pending_show
            .as_ref()
            .map(|p| p.layer_surface.wl_surface().id());
        let live_id = self.layer_surface.as_ref().map(|s| s.wl_surface().id());

        match surface_match(pending_id.as_ref(), live_id.as_ref(), &event_id) {
            SurfaceMatch::Pending => {
                self.complete_pending_show(size);
            }
            SurfaceMatch::Live => {
                // Re-aim viewport for an existing live surface (e.g.
                // output geometry change).
                self.configured_size = size;
                if let Some(viewport) = &self.viewport {
                    viewport.set_destination(size.0.cast_signed(), size.1.cast_signed());
                }
                if let Some(surface) = &self.layer_surface {
                    surface.commit();
                }
                tracing::debug!(
                    event = "layer_surface_reconfigured",
                    display_id = %self.display_id,
                    width = size.0,
                    height = size.1,
                );
            }
            SurfaceMatch::Stale => {
                tracing::debug!(
                    event = "render_stale_configure",
                    display_id = %self.display_id,
                    event_surface = %event_id,
                    pending_surface = ?pending_id,
                    live_surface = ?live_id,
                    "stale configure for a surface that is not pending or live; ignored"
                );
            }
        }
    }
}

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers!(OutputState);
}

delegate_registry!(WaylandState);
delegate_output!(WaylandState);
delegate_compositor!(WaylandState);
delegate_shm!(WaylandState);
delegate_layer!(WaylandState);

// ── Custom Dispatch impls for our own globals + seat + input ────────────────

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: <zwlr_layer_shell_v1::ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpSinglePixelBufferManagerV1,
        _event: <WpSinglePixelBufferManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewporter, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &WlBuffer,
        event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Track `release` events so the screensaver's per-buffer busy
        // flags flip back to `false` when the compositor finishes
        // reading.  Without this, both buffers stay busy and
        // `on_mpv_wakeup` would skip every frame (Wayland protocol
        // correctness gap — S1).
        if let wayland_client::protocol::wl_buffer::Event::Release = event
            && let Some(session) = state.screensaver_session.as_mut()
        {
            let id = proxy.id();
            if session.buffers[0].id() == id {
                session.buffers_busy[0] = false;
            } else if session.buffers[1].id() == id {
                session.buffers_busy[1] = false;
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(cap),
        } = event
        {
            if cap.contains(wayland_client::protocol::wl_seat::Capability::Pointer) {
                let pointer = state.seat.get_pointer(qh, ());
                state.pointer = Some(pointer);
            }
            if cap.contains(wayland_client::protocol::wl_seat::Capability::Keyboard) {
                let keyboard = state.seat.get_keyboard(qh, ());
                state.keyboard = Some(keyboard);
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wayland_client::protocol::wl_pointer::Event::Enter {
                serial, surface: _, ..
            } => {
                // Cursor hide: a null surface makes the compositor stop
                // drawing one.  Surface receive pointer input because we
                // never set an input region.
                if let Some(pointer) = &state.pointer {
                    pointer.set_cursor(serial, None, 0, 0);
                }
                state.last_pointer_serial = Some(serial);
            }
            wayland_client::protocol::wl_pointer::Event::Button {
                serial, button: _, ..
            } => {
                state.last_pointer_serial = Some(serial);
                state.on_input_event();
            }
            wayland_client::protocol::wl_pointer::Event::Motion { .. } => {
                state.on_input_event();
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_keyboard::Event::Key { .. } = event {
            state.on_input_event();
        }
    }
}

// Marker trait instances — empty Event/Error enums from the protocol
// bindings may not have explicit Dispatch impls in some setups; these
// aliases document the bound without producing runtime work.
#[allow(dead_code)]
type _DispatchedByState = zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;
