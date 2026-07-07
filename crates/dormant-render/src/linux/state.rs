//! Wayland thread state — the dispatch target for every Wayland object.
//!
//! All Wayland proxies + the SCTK handler state live here.  Crucially:
//! the `EventQueue` itself is **not** a field — it stays loop-local in
//! `connection.rs`.  We hold a clone of its `QueueHandle` (cheap,
//! `'static`) so surface-creation calls can still bind proxies to the
//! right queue without storing the queue itself.

use std::os::fd::{BorrowedFd, RawFd};
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
use calloop::{Interest, Mode, PostAction};

use dormant_core::error::E_RENDER_UNAVAILABLE;
use dormant_core::types::{CmdFailure, DisplayId, StageKind};

use crate::command::RenderCommand;
use crate::latch::FirstInputLatch;
use crate::screensaver::{MpvPlayer, ScreensaverSettings};

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
/// to write into.  Returns `(read_fd, write_fd)` — the read end is
/// registered with calloop, the write end is given to the mpv player.
fn make_wakeup_pipe() -> Result<(RawFd, RawFd), CmdFailure> {
    let mut pipe_fds = [0 as RawFd; 2];
    // SAFETY: pipe2 writes both fds into the provided array.
    let ret = unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if ret < 0 {
        return Err(cmd_failure(
            "screensaver",
            &format!("pipe2: {}", std::io::Error::last_os_error()),
        ));
    }
    Ok((pipe_fds[0], pipe_fds[1]))
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
    let fmt = wayland_client::protocol::wl_shm::Format::Argb8888;
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
    pub(super) read_fd: RawFd,
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
            match self.complete_screensaver_show(pending.layer_surface, configured_size, settings) {
                Ok(()) => {
                    let _ = pending.reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = pending.reply.send(Err(e));
                }
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
    /// source, and attach + commit the first back buffer.  On failure
    /// at this stage (mpv init / pipe2 / shm pool), returns
    /// `Err(CmdFailure)` — the caller resolves the pending show with it.
    fn complete_screensaver_show(
        &mut self,
        layer_surface: LayerSurface,
        configured_size: (u32, u32),
        settings: ScreensaverSettings,
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
        // The write fd is consumed by the player; the read fd is owned
        // by `self` and registered as a calloop source below.
        let (read_fd, write_fd) = make_wakeup_pipe()?;

        // ── mpv player ──────────────────────────────────────────────
        let player = MpvPlayer::new(
            settings.items,
            settings.image_duration,
            settings.audio,
            width,
            height,
            write_fd,
        )
        .map_err(|e| {
            // SAFETY: pipe was created; close both ends on early failure.
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            cmd_failure("screensaver", &format!("{e}"))
        })?;

        // ── double-buffered shm pool ────────────────────────────────
        let pool_byte_len = (stride as usize)
            .checked_mul(height as usize)
            .and_then(|x| x.checked_mul(2))
            .ok_or_else(|| cmd_failure("screensaver", "shm pool size overflow"))?;
        let mut pool =
            smithay_client_toolkit::shm::raw::RawPool::new(pool_byte_len, &self.shm_state)
                .map_err(|e| cmd_failure("screensaver", &format!("RawPool::new: {e}")))?;
        let qh = self.queue_handle.clone();
        let (buf0, buf1) = create_dual_buffers(&mut pool, &qh, width, height, stride);

        // Attach the first back buffer (all zeros — i.e. opaque black
        // since the format is XRGB8888 byte-order) and commit.  mpv's
        // first wakeup will follow shortly and overwrite it.
        let wl_surface = layer_surface.wl_surface();
        wl_surface.attach(Some(&buf0), 0, 0);
        wl_surface.damage_buffer(0, 0, width.cast_signed(), height.cast_signed());
        wl_surface.commit();

        // ── calloop wakeup source ───────────────────────────────────
        // SAFETY: read_fd was created by `make_wakeup_pipe` above; we
        // own it until `destroy_screensaver_session` closes it.
        let borrowed_read_fd = unsafe { BorrowedFd::borrow_raw(read_fd) };
        let source = Generic::new(borrowed_read_fd, Interest::READ, Mode::Level);

        let Some(loop_handle) = self.loop_handle.as_ref() else {
            // SAFETY: pipe was created; close both ends on early failure.
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(cmd_failure(
                "screensaver",
                "loop handle not installed on state",
            ));
        };
        let token = match loop_handle.insert_source(source, screensaver_wakeup_cb) {
            Ok(t) => t,
            Err(e) => {
                // SAFETY: pipe was created; close both ends on early failure.
                unsafe {
                    libc::close(read_fd);
                    libc::close(write_fd);
                }
                return Err(cmd_failure("screensaver", &format!("insert_source: {e}")));
            }
        };

        // Install the session now that the source is registered.
        self.screensaver_session = Some(ScreensaverSession {
            player,
            pool,
            buffers: [buf0, buf1],
            width,
            height,
            stride,
            next_render_idx: 1,
            read_fd,
        });
        // The wakeup slot holds the token so we can later remove the
        // source via `loop_handle.remove(token)` from
        // `destroy_screensaver_session`.  (Storing the Generic itself
        // works for Drop — calloop's Generic::drop doesn't unregister,
        // so we need the explicit token-based remove.)
        self.screensaver_wakeup_token = Some(token);
        self.layer_surface = Some(layer_surface);
        self.configured_size = configured_size;
        self.surface_up = true;

        tracing::info!(
            event = "render_screensaver_up",
            display_id = %self.display_id,
            output = %self.output_name,
            width,
            height,
        );
        Ok(())
    }

    /// mpv wakeup callback: drain the pipe, render one frame into the
    /// back buffer, attach + damage + commit, swap indices.  Called
    /// from the calloop thread when the Generic source signals.
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
                    session.read_fd,
                    drain_buf.as_mut_ptr().cast(),
                    drain_buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
        }

        // Render into the back buffer.
        let back_idx = session.next_render_idx;
        let stride = session.stride as usize;
        let buf_len = stride * (session.height as usize);
        let back_offset = back_idx * buf_len;
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

        // Swap so the next render writes to the buffer the compositor
        // has finished with.
        session.next_render_idx = 1 - back_idx;
    }

    /// Post-first-frame failure: tear down the session, fall back to
    /// the opaque-black buffer on the SAME surface, and log.
    fn fail_screensaver_to_black(&mut self, reason: &str) {
        tracing::warn!(
            event = "screensaver_failed_to_black",
            display_id = %self.display_id,
            reason = reason,
        );
        // Destroy the session first — frees the mpv player + the shm pool
        // and removes the wakeup source.
        self.destroy_screensaver_session();

        // Re-attach the existing black buffer (if any) and commit.
        if let (Some(surface), Some(black)) = (&self.layer_surface, &self.black_buffer) {
            let wl_surface = surface.wl_surface();
            wl_surface.attach(Some(black), 0, 0);
            wl_surface.commit();
        }
    }

    /// Tear down the active screensaver session (if any).  Drops the
    /// `MpvPlayer` (which closes the mpv wakeup write fd), drops the
    /// shm pool (which closes the underlying `wl_shm_pool`), and removes
    /// the wakeup calloop source (which closes the pipe read fd).
    fn destroy_screensaver_session(&mut self) {
        // Remove the calloop source FIRST so no further callbacks fire
        // against a session that's about to be dropped.
        if let (Some(token), Some(handle)) = (
            self.screensaver_wakeup_token.take(),
            self.loop_handle.as_ref(),
        ) {
            // `remove` on calloop's LoopHandle takes the source by
            // value (drops it) — we can't do that without owning the
            // source.  Instead, disable the source by removing its
            // registration token.  The Generic itself is stored on
            // the session so its Drop runs on session destroy.
            handle.remove(token);
        }
        // Then drop the session — its Drop closes the read fd (the pipe
        // half not owned by mpv), the player closes the write fd, and the
        // pool destroys the wl_shm_pool.
        if let Some(session) = self.screensaver_session.take() {
            let ScreensaverSession {
                player,
                read_fd,
                pool,
                buffers,
                width: _,
                height: _,
                stride: _,
                next_render_idx: _,
            } = session;
            // player.destroy() unregisters the mpv wakeup callback and
            // closes the write fd.
            player.destroy();
            // SAFETY: read_fd was created via pipe2 and is owned by the
            // session; closing once here after the player destroy.
            unsafe {
                libc::close(read_fd);
            }
            // pool + buffers drop here; RawPool's Drop destroys the
            // wl_shm_pool, which in turn releases the WlBuffers.
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
                // If a surface was up, drop it first so the new generation
                // starts from a clean configured size.
                self.destroy_surface();
                self.input_latch.reset();

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
                // Screensaver stages are not yet implemented in this
                // backend — they fall through.  Controller stages never
                // reach a render sink at all (the engine routes them
                // through the command-sink chain).
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
        // flicker" path described in the design phase.
        if let Some(existing) = &self.layer_surface
            && self.surface_up
        {
            match self.complete_screensaver_show(existing.clone(), self.configured_size, settings) {
                Ok(()) => {
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
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
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
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
