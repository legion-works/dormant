//! The dedicated Wayland thread.
//!
//! Owns the compositor [`Connection`], the layer surface, the input
//! latch, and a calloop [`EventLoop`] that pumps two sources:
//!
//! - the [`RenderCommand`] channel used by the async side of the sink.
//!
//! Wayland I/O is driven *inline* inside the channel callback — the
//! callback that processes a `Show` / `Teardown` calls
//! `event_queue.roundtrip()` directly to wait for the compositor's
//! `configure` reply before replying to the caller.  This avoids a
//! race where the calloop loop's FD read and `roundtrip`'s read
//! compete for the same Wayland socket.
//!
//! The thread is built by [`spawn_wayland_thread`]: it connects to the
//! compositor, binds the required globals, runs an initial roundtrip to
//! populate output names, locates the target output, then returns the
//! command `Sender` to the caller.  If any step fails it returns a
//! [`CmdFailure`] describing the failure; the spawned thread exits and
//! drops its connection.

use std::time::Duration;

use calloop::channel::{Channel, Sender, channel};
use calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
};

// `Backend` is the trait that provides `Connection::backend()`.  Rust's
// unused-import lint doesn't see trait-method uses, so silence the false
// positive here.
#[allow(unused_imports)]
use wayland_client::backend::ReadEventsGuard;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_keyboard::{Event as WlKeyboardEvent, WlKeyboard};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::{Event as WlPointerEvent, WlPointer};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;

use tokio::sync::mpsc::UnboundedSender;

use dormant_core::error::E_RENDER_UNAVAILABLE;
use dormant_core::types::{CmdFailure, DisplayId, StageKind};

use crate::command::RenderCommand;
use crate::latch::FirstInputLatch;

/// Opaque black in `u32` ARGB host order (matches `create_u32_rgba_buffer`).
const OPAQUE_BLACK_U32: u32 = 0xFF00_0000;

/// Default namespace for the layer surface — visible in `wayland-info`.
const LAYER_NAMESPACE: &str = "dormant";

/// Maximum time we'll wait for a compositor `configure` event after the
/// initial layer-surface commit.  Compositors are expected to respond
/// in single-digit milliseconds; 2 seconds is comfortable slack without
/// masking a genuine hang.
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(2);

// ── Entry point ───────────────────────────────────────────────────────────────

/// Spawn the dedicated Wayland thread and return a [`Sender`] that the
/// handle uses to enqueue [`RenderCommand`]s.
///
/// The thread owns one compositor connection for the lifetime of the
/// sink.  On any bind / connect / target-output failure this function
/// returns a [`CmdFailure`] without leaving a zombie thread behind.
///
/// `input_wake_tx` is forwarded to the thread so the first pointer or
/// key event on the active surface pushes the display id through.
pub(super) fn spawn_wayland_thread(
    display_id: &DisplayId,
    output_name: &str,
    input_wake_tx: Option<&UnboundedSender<DisplayId>>,
) -> Result<Sender<RenderCommand>, CmdFailure> {
    // `EventLoop` is `!Send` (calloop uses `Rc` internally), so we
    // build *everything* inside the spawned thread and use a oneshot
    // sync channel to ferry the command sender back.  The thread keeps
    // running the loop after init succeeds; on init failure it exits
    // and drops the connection.
    let (init_tx, init_rx) =
        std::sync::mpsc::sync_channel::<Result<Sender<RenderCommand>, CmdFailure>>(1);

    let did = display_id.clone();
    let oname = output_name.to_string();
    let iwt = input_wake_tx.cloned();

    std::thread::Builder::new()
        .name(format!("dormant-render-{display_id}"))
        .spawn(move || {
            let result = init_and_run(&did, &oname, iwt.as_ref());
            match result {
                Ok((cmd_tx, event_loop, state)) => {
                    // Clone the sender so we can ferry a copy to the
                    // caller AND keep one alive in the wayland thread
                    // (the calloop source observes `Event::Closed`
                    // when the last sender drops).
                    let caller_tx = cmd_tx.clone();
                    let _ = init_tx.send(Ok(caller_tx));
                    run_loop(event_loop, state, cmd_tx);
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: spawn wayland thread: {e}"),
        })?;

    init_rx.recv().map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: init channel closed: {e}"),
    })?
}

/// Initialise the connection, bind the globals, locate the target
/// output, wire the event sources, then drop into the dispatch loop.
/// Returns the [`Sender`] end of the command channel on success.
fn init_and_run(
    display_id: &DisplayId,
    output_name: &str,
    input_wake_tx: Option<&UnboundedSender<DisplayId>>,
) -> Result<
    (
        Sender<RenderCommand>,
        EventLoop<'static, WaylandState>,
        WaylandState,
    ),
    CmdFailure,
> {
    let conn = Connection::connect_to_env().map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: connect_to_env: {e}"),
    })?;

    let (globals, event_queue) =
        registry_queue_init::<WaylandState>(&conn).map_err(|e| CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: registry_queue_init: {e}"),
        })?;
    let qh = event_queue.handle();

    // Core globals — compositor / shm / output / layer-shell are required.
    let compositor_state = CompositorState::bind(&globals, &qh).map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: compositor bind: {e}"),
    })?;
    let shm_state = Shm::bind(&globals, &qh).map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: shm bind: {e}"),
    })?;
    let output_state = OutputState::new(&globals, &qh);
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: layer_shell bind: {e}"),
    })?;

    // Staging globals — single-pixel-buffer + viewporter are preferred.
    // They're optional at the bind step; we degrade to shm fallback when
    // a compositor omits one or both.
    let single_pixel_manager: Option<WpSinglePixelBufferManagerV1> =
        globals.bind(&qh, 1..=1, ()).ok();
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    tracing::debug!(
        event = "render_globals_bound",
        single_pixel = single_pixel_manager.is_some(),
        viewporter = viewporter.is_some(),
    );

    // The `wl_seat` global is required so we can receive pointer/keyboard
    // input for the wake grab + cursor hide.  Treat its absence as fatal.
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: wl_seat bind: {e}"),
    })?;

    let event_loop: EventLoop<'static, WaylandState> =
        EventLoop::try_new().map_err(|e| CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: calloop event loop: {e}"),
        })?;
    let loop_handle = event_loop.handle();

    let registry_state = RegistryState::new(&globals);
    let mut state = WaylandState::new(
        event_queue,
        compositor_state,
        shm_state,
        output_state,
        layer_shell,
        single_pixel_manager,
        viewporter,
        seat,
        registry_state,
        display_id.clone(),
        output_name.to_string(),
        input_wake_tx.cloned(),
    );

    // First roundtrip populates output info (`OutputInfo::name` is only
    // valid after a roundtrip — see spike gotcha #6).
    if let Err(e) = state.roundtrip() {
        return Err(CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: initial roundtrip: {e}"),
        });
    }

    // Locate the target output by connector name.
    state.locate_target_output()?;

    // Wire only the command channel source.  Wayland I/O is driven
    // inline from inside the channel callback (via `state.roundtrip`)
    // — see module docs for why we don't add a Wayland-FD source here.
    let (cmd_tx, cmd_rx) = channel::<RenderCommand>();
    install_command_source(&loop_handle, cmd_rx);

    Ok((cmd_tx, event_loop, state))
}

/// Drive the calloop dispatch forever.  Exits when the loop signals an
/// unrecoverable error or when `cmd_tx` is dropped (channel closes,
/// triggering [`calloop::channel::Event::Closed`] in the callback, which
/// the wayland thread treats as a signal to break).
fn run_loop(
    mut event_loop: EventLoop<'static, WaylandState>,
    mut state: WaylandState,
    cmd_tx: Sender<RenderCommand>,
) {
    // Hold `cmd_tx` so the channel stays open; dropping it would make
    // the calloop source observe `Event::Closed` and the loop would
    // exit.  The async sink also keeps its own clone alive.
    let _keep_alive = cmd_tx;

    tracing::info!(
        event = "wayland_thread_started",
        display_id = %state.display_id,
        output_name = %state.output_name,
    );

    loop {
        if let Err(e) = event_loop.dispatch(Some(Duration::from_millis(500)), &mut state) {
            tracing::error!(
                event = "wayland_loop_dispatch_error",
                error = %e,
                display_id = %state.display_id,
            );
            break;
        }
    }
    tracing::info!(event = "wayland_thread_exit", display_id = %state.display_id);
}

fn install_command_source(handle: &LoopHandle<'static, WaylandState>, rx: Channel<RenderCommand>) {
    if let Err(e) = handle.insert_source(rx, |event, (), state: &mut WaylandState| {
        if let calloop::channel::Event::Msg(cmd) = event {
            state.handle_command(cmd);
        }
    }) {
        tracing::error!(event = "command_source_insert_failed", error = %e);
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

/// All Wayland-side state owned by the dedicated thread.  Holds the
/// `EventQueue`, layer surface, input latch, and pending command
/// replies.  The `Connection` itself stays local to the spawn function —
/// every operation that needs it goes through `event_queue`.
pub(super) struct WaylandState {
    // ── Connection plumbing ────────────────────────────────────────────────
    pub(super) event_queue: EventQueue<WaylandState>,

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
    #[allow(dead_code)]
    pub(super) last_pointer_serial: Option<u32>,

    // ── Per-display config ─────────────────────────────────────────────────
    pub(super) display_id: DisplayId,
    pub(super) output_name: String,
    pub(super) input_wake_tx: Option<UnboundedSender<DisplayId>>,

    // ── Live layer surface ─────────────────────────────────────────────────
    pub(super) target_output: Option<WlOutput>,
    pub(super) layer_surface: Option<LayerSurface>,
    pub(super) viewport: Option<WpViewport>,
    pub(super) black_buffer: Option<WlBuffer>,
    pub(super) configured_size: (u32, u32),
    pub(super) surface_up: bool,

    // ── First-input latch ──────────────────────────────────────────────────
    pub(super) input_latch: FirstInputLatch,
}

impl WaylandState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        event_queue: EventQueue<Self>,
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
        input_wake_tx: Option<UnboundedSender<DisplayId>>,
    ) -> Self {
        let input_latch = FirstInputLatch::new(display_id.clone());
        Self {
            event_queue,
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
            input_wake_tx,
            target_output: None,
            layer_surface: None,
            viewport: None,
            black_buffer: None,
            configured_size: (0, 0),
            surface_up: false,
            input_latch,
        }
    }

    // ── Borrow-splitting helpers for `EventQueue` ─────────────────────────
    //
    // `EventQueue::roundtrip` / `dispatch_pending` / `prepare_read` take
    // `&mut self` for the queue and `&mut State`.  Both reference the
    // same struct, so the borrow checker rejects the direct call.  The
    // raw-pointer escape is safe because the three methods only touch
    // the `EventQueue` (and dispatch into `state` via user callbacks);
    // they never touch other `WaylandState` fields.  The pointer's
    // provenance is tied to `&mut self` so no other live mutable
    // borrow can coexist.

    fn roundtrip(&mut self) -> Result<usize, wayland_client::DispatchError> {
        let queue = std::ptr::addr_of_mut!(self.event_queue);
        let _ = unsafe { (*queue).dispatch_pending(self) };
        // The above drains queued events; now do a blocking read+dispatch
        // round so any pending requests are answered.  We swallow the
        // dispatch count and only care about the final error.
        unsafe { (*queue).roundtrip(self) }
    }

    fn dispatch_pending(&mut self) -> Result<usize, wayland_client::DispatchError> {
        let queue = std::ptr::addr_of_mut!(self.event_queue);
        unsafe { (*queue).dispatch_pending(self) }
    }

    fn prepare_read(&mut self) -> Option<ReadEventsGuard> {
        let queue = std::ptr::addr_of_mut!(self.event_queue);
        // SAFETY: the guard borrows the connection's backend, which
        // lives for the entire thread.  The returned guard's lifetime
        // extends past our pointer dereference but we always drop it
        // before any other use of self.
        unsafe { (*queue).prepare_read() }
    }

    // ── Domain methods ─────────────────────────────────────────────────────

    /// Walk the bound outputs and pick the one whose `OutputInfo::name`
    /// matches `output_name`.  Called after the initial roundtrip has
    /// populated the output info.
    fn locate_target_output(&mut self) -> Result<(), CmdFailure> {
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

    /// Apply a [`RenderCommand::Show`] to the live state.  Returns a
    /// reply through the embedded oneshot sender.
    fn handle_show(
        &mut self,
        r#gen: u64,
        _idx: usize,
        kind: StageKind,
        reply: tokio::sync::oneshot::Sender<Result<(), CmdFailure>>,
    ) {
        let result = match kind {
            StageKind::RenderBlack => self.show_black(r#gen),
            StageKind::RenderScreensaver | StageKind::Controller(_) => {
                // Screensaver is Phase 2; controllers don't reach render.
                // Fall-through contract: error so the engine can advance.
                Err(CmdFailure {
                    controller: "render-black".into(),
                    error: format!(
                        "{E_RENDER_UNAVAILABLE}: stage {kind:?} not implemented in this backend"
                    ),
                })
            }
        };
        let _ = reply.send(result);
    }

    /// Create / refresh the black overlay for `r#gen`.  Idempotent on
    /// repeat `Show` calls — the existing surface is destroyed first to
    /// pick up any output / scale changes.
    fn show_black(&mut self, r#gen: u64) -> Result<(), CmdFailure> {
        // Tear down any existing surface first so the new gen starts
        // from a clean state.
        if self.surface_up {
            self.destroy_surface();
        }
        self.input_latch.reset();

        let target_output = self.target_output.clone().ok_or_else(|| CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: no target output bound"),
        })?;
        let qh = self.queue_handle();

        // Create the layer surface.  This issues an initial commit
        // (spike gotcha #7) which triggers a configure.
        let surface = self.compositor_state.create_surface(&qh);
        let layer_surface = self.layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some(LAYER_NAMESPACE),
            Some(&target_output),
        );
        layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer_surface.commit();

        // Block on the compositor's configure reply.  `EventQueue::roundtrip`
        // flushes pending requests, blocks reading from the socket, and
        // dispatches incoming events until the request has a matching
        // reply.  Configure is the matching reply for the initial commit,
        // so by the time roundtrip returns `self.configured_size` is set.
        // A bounded wait guards against a stuck compositor.
        let deadline = std::time::Instant::now() + CONFIGURE_TIMEOUT;
        while self.configured_size == (0, 0) {
            if std::time::Instant::now() >= deadline {
                return Err(CmdFailure {
                    controller: "render-black".into(),
                    error: format!(
                        "{E_RENDER_UNAVAILABLE}: compositor did not configure layer surface in {CONFIGURE_TIMEOUT:?}"
                    ),
                });
            }
            self.roundtrip().map_err(|e| CmdFailure {
                controller: "render-black".into(),
                error: format!("{E_RENDER_UNAVAILABLE}: configure roundtrip: {e}"),
            })?;
        }

        let (w, h) = self.configured_size;
        if w == 0 || h == 0 {
            return Err(CmdFailure {
                controller: "render-black".into(),
                error: format!(
                    "{E_RENDER_UNAVAILABLE}: configured size is 0×0 — compositor refused to size surface"
                ),
            });
        }

        // Attach the opaque-black buffer.  Prefer single-pixel + viewport;
        // fall back to a shm buffer when those globals are unavailable.
        let wl_surface = layer_surface.wl_surface();
        let (buffer, viewport) =
            if let (Some(spm), Some(vp)) = (&self.single_pixel_manager, &self.viewporter) {
                attach_single_pixel_black(spm, vp, wl_surface, w, h, &qh)
            } else {
                // shm fallback path — keeps the build usable on
                // compositors that omit the staging globals.
                let mut pool = smithay_client_toolkit::shm::raw::RawPool::new(
                    (w as usize) * (h as usize) * 4,
                    &self.shm_state,
                )
                .map_err(|e| CmdFailure {
                    controller: "render-black".into(),
                    error: format!("{E_RENDER_UNAVAILABLE}: shm pool: {e}"),
                })?;
                {
                    let mmap = pool.mmap();
                    let pixel = OPAQUE_BLACK_U32.to_ne_bytes();
                    for row in 0..h as usize {
                        let row_start = row * (w as usize) * 4;
                        for col in 0..(w as usize) {
                            let offset = row_start + col * 4;
                            mmap[offset..offset + 4].copy_from_slice(&pixel);
                        }
                    }
                }
                let buffer = pool.create_buffer(
                    0,
                    w.cast_signed(),
                    h.cast_signed(),
                    (w.cast_signed()) * 4,
                    wayland_client::protocol::wl_shm::Format::Argb8888,
                    (),
                    &qh,
                );
                wl_surface.attach(Some(&buffer), 0, 0);
                wl_surface.commit();
                (buffer, None)
            };

        self.layer_surface = Some(layer_surface);
        self.viewport = viewport;
        self.black_buffer = Some(buffer);
        self.surface_up = true;
        self.configured_size = (w, h);

        tracing::info!(
            event = "render_black_up",
            display_id = %self.display_id,
            output = %self.output_name,
            r#gen,
            width = w,
            height = h,
        );
        Ok(())
    }

    /// Drain any pending Wayland events without blocking.  Used to
    /// service `configure` callbacks without involving the calloop loop.
    #[allow(dead_code)] // kept for future use; show_black uses roundtrip directly.
    fn drain_pending_events(&mut self) {
        loop {
            match self.dispatch_pending() {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            match self.prepare_read() {
                Some(guard) => {
                    if guard.read().is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    }

    /// Borrow `QueueHandle` for the embedded queue without aliasing
    /// `&mut self`.  Safe because `QueueHandle::new` (the only way to
    /// obtain a handle) takes `&mut EventQueue`, and we extract the
    /// pointer the same way as the other helpers above.
    fn queue_handle(&mut self) -> QueueHandle<WaylandState> {
        let queue = std::ptr::addr_of_mut!(self.event_queue);
        // SAFETY: same as the other helpers — the handle is a thin
        // wrapper that internally borrows the queue; we keep the
        // queue alive for the lifetime of the thread.
        unsafe { (*queue).handle() }
    }

    /// Drop any live surface state and commit a no-buffer so the
    /// compositor releases it.  Idempotent.
    fn destroy_surface(&mut self) {
        if let Some(surface) = self.layer_surface.take() {
            let wl_surface = surface.wl_surface();
            wl_surface.attach(None, 0, 0);
            wl_surface.commit();
            // Drop the LayerSurface proxy — its destruction is the
            // protocol signal that the surface is gone.
        }
        self.viewport = None;
        self.black_buffer = None;
        self.surface_up = false;
        self.configured_size = (0, 0);
        self.input_latch.reset();
    }

    /// Apply a [`RenderCommand::Teardown`].
    fn handle_teardown(&mut self, r#gen: u64, reply: tokio::sync::oneshot::Sender<()>) {
        self.destroy_surface();
        // Flush the no-buffer commit.  Errors are non-fatal here —
        // the next event-loop tick will pick up anything queued.
        let _ = self.roundtrip();
        tracing::info!(
            event = "render_teardown",
            display_id = %self.display_id,
            output = %self.output_name,
            r#gen,
        );
        let _ = reply.send(());
    }

    /// Dispatch entry for incoming commands.
    pub(super) fn handle_command(&mut self, cmd: RenderCommand) {
        match cmd {
            RenderCommand::Show {
                r#gen,
                idx,
                kind,
                reply,
            } => self.handle_show(r#gen, idx, kind, reply),
            RenderCommand::Teardown { r#gen, reply } => self.handle_teardown(r#gen, reply),
        }
    }

    /// Register an input event from the pointer / keyboard handler.
    /// First event after a surface-up fires `InputWake`; subsequent
    /// events are silently dropped.
    fn on_input_event(&mut self) {
        if !self.surface_up {
            // Surface torn down — drop.  Latch stays consumed until
            // the next Show.
            return;
        }
        if let (Some(display_id), Some(tx)) = (self.input_latch.on_input(), &self.input_wake_tx) {
            let _ = tx.send(display_id);
        }
    }
}

// ── SCTK delegate impls ───────────────────────────────────────────────────────

// The state type needs the standard SCTK handler trait impls so the
// `delegate_*!` macros can emit Dispatch impls for the registered globals.

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
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        // Compositor closed the layer surface externally — flush our
        // bookkeeping.
        self.surface_up = false;
        self.layer_surface = None;
        self.viewport = None;
        self.black_buffer = None;
        self.configured_size = (0, 0);
        tracing::info!(
            event = "layer_surface_closed_by_compositor",
            display_id = %self.display_id,
        );
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(w.cast_signed(), h.cast_signed());
        }
        self.configured_size = (w, h);
        // Commit the configured size.  The buffer may already be
        // attached (we attach after a previous configure round-trip),
        // in which case this just refreshes the size.
        layer.commit();
        tracing::debug!(
            event = "layer_surface_configured",
            display_id = %self.display_id,
            width = w,
            height = h,
        );
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

// ── Custom Dispatch impls ─────────────────────────────────────────────────────

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
        qh: &QueueHandle<Self>,
    ) {
        match event {
            WlPointerEvent::Enter {
                serial, surface, ..
            } => {
                // Cursor hide: setting an empty cursor with a null
                // surface makes the compositor stop drawing one.
                if let Some(pointer) = &state.pointer {
                    pointer.set_cursor(serial, None, 0, 0);
                }
                state.last_pointer_serial = Some(serial);
                let _ = (qh, surface);
                // Pointer enter alone is not the wake — it's almost
                // always the cursor passing over the surface during
                // activation.  Real wake comes from button / key events.
            }
            WlPointerEvent::Button { serial, button, .. } => {
                state.last_pointer_serial = Some(serial);
                let _ = button;
                state.on_input_event();
            }
            WlPointerEvent::Motion { .. } => {
                // Pointer motion is a wake candidate, but the spec for
                // the input latch says "first event of any kind".  Use
                // motion too — even without a click, the user's hand
                // is on the mouse.
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
        qh: &QueueHandle<Self>,
    ) {
        if let WlKeyboardEvent::Key { .. } = event {
            state.on_input_event();
        }
        let _ = qh;
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn attach_single_pixel_black(
    single_pixel_manager: &WpSinglePixelBufferManagerV1,
    viewporter: &WpViewporter,
    wl_surface: &WlSurface,
    width: u32,
    height: u32,
    qh: &QueueHandle<WaylandState>,
) -> (WlBuffer, Option<WpViewport>) {
    let buffer =
        single_pixel_manager.create_u32_rgba_buffer(OPAQUE_BLACK_U32, 0, 0, u32::MAX, qh, ());
    let viewport = viewporter.get_viewport(wl_surface, qh, ());
    viewport.set_destination(width.cast_signed(), height.cast_signed());
    wl_surface.attach(Some(&buffer), 0, 0);
    wl_surface.commit();
    (buffer, Some(viewport))
}

// `GlobalList` is referenced only by the RegistryState constructor that
// runs at startup; we don't touch it from this module.  The type is
// imported transitively via `registry_queue_init`.
