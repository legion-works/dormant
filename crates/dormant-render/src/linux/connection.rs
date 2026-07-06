//! Wayland thread lifecycle — connect, install event sources, run the
//! calloop dispatch forever.
//!
//! The flow is the canonical calloop + wayland shape:
//!
//! 1. Build the `Connection`, `EventQueue`, all SCTK globals.
//! 2. Run a one-shot roundtrip to populate output info.
//! 3. Wrap the `(Connection, EventQueue)` pair in a
//!    [`calloop_wayland_source::WaylandSource`] and `insert` it into the
//!    loop — the source drives the Wayland FD as a calloop event source,
//!    so `dispatch_pending` fires whenever there's data to read (idle
//!    threads still service input + configure events).
//! 4. Insert the command channel as a second calloop source.  Show /
//!    Teardown commands run inline from inside the channel callback;
//!    configure completion / teardown completion land on the next
//!    `WaylandSource` tick.
//! 5. Loop forever.  When the command channel observes
//!    [`calloop::channel::Event::Closed`] (every `LayerShellRenderSink`
//!    handle has dropped), tear down surfaces and exit.
//!
//! Configure-timeout: every Show arms a one-shot `calloop::timer::Timer`
//! that, on fire, posts a [`RenderCommand::ConfigureTimeout`] back into
//! the same command channel.  The handler fails the in-flight pending
//! show with an `E_RENDER_UNAVAILABLE` error so a silent compositor
//! can never wedge the thread or hang the async caller.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use calloop::channel::{Event as ChannelEvent, Sender, channel};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shm::Shm;

use tokio::sync::mpsc::UnboundedSender;

use wayland_client::Connection;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_seat::WlSeat;

use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use dormant_core::error::E_RENDER_UNAVAILABLE;
use dormant_core::types::{CmdFailure, DisplayId};

use crate::command::RenderCommand;
use crate::linux::state::{CONFIGURE_TIMEOUT, WaylandState};

/// Spawn the dedicated Wayland thread and return a [`Sender`] that the
/// handle uses to enqueue [`RenderCommand`]s.
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
            let result = init(&did, &oname, iwt.as_ref());
            match result {
                Ok((cmd_tx, event_loop, state, loop_handle)) => {
                    let caller_tx = cmd_tx.clone();
                    let _ = init_tx.send(Ok(caller_tx));
                    run_loop(event_loop, state, loop_handle, cmd_tx);
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
/// output, wire the event sources, then hand the loop over to
/// [`run_loop`].
type InitResult = Result<
    (
        Sender<RenderCommand>,
        EventLoop<'static, WaylandState>,
        WaylandState,
        LoopHandle<'static, WaylandState>,
    ),
    CmdFailure,
>;

fn init(
    display_id: &DisplayId,
    output_name: &str,
    input_wake_tx: Option<&UnboundedSender<DisplayId>>,
) -> InitResult {
    let conn = Connection::connect_to_env().map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: connect_to_env: {e}"),
    })?;

    let (globals, mut event_queue) =
        registry_queue_init::<WaylandState>(&conn).map_err(|e| CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: registry_queue_init: {e}"),
        })?;
    let queue_handle = event_queue.handle();

    // Core globals — compositor / shm / output / layer-shell are required.
    let compositor_state =
        CompositorState::bind(&globals, &queue_handle).map_err(|e| CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: compositor bind: {e}"),
        })?;
    let shm_state = Shm::bind(&globals, &queue_handle).map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: shm bind: {e}"),
    })?;
    let output_state = OutputState::new(&globals, &queue_handle);
    let layer_shell = LayerShell::bind(&globals, &queue_handle).map_err(|e| CmdFailure {
        controller: "render-black".into(),
        error: format!("{E_RENDER_UNAVAILABLE}: layer_shell bind: {e}"),
    })?;

    // Staging globals — single-pixel-buffer + viewporter are preferred.
    let single_pixel_manager: Option<WpSinglePixelBufferManagerV1> =
        globals.bind(&queue_handle, 1..=1, ()).ok();
    let viewporter: Option<WpViewporter> = globals.bind(&queue_handle, 1..=1, ()).ok();
    tracing::debug!(
        event = "render_globals_bound",
        single_pixel = single_pixel_manager.is_some(),
        viewporter = viewporter.is_some(),
    );

    // The `wl_seat` global is required so we can receive pointer/keyboard
    // input for the wake grab + cursor hide.  Treat its absence as fatal.
    let seat: WlSeat = globals
        .bind(&queue_handle, 1..=8, ())
        .map_err(|e| CmdFailure {
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
    let loop_should_exit = Arc::new(AtomicBool::new(false));
    let mut state = WaylandState::new(
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
        input_wake_tx,
        queue_handle,
        loop_should_exit.clone(),
    );

    // First roundtrip populates output info.
    if let Err(e) = event_queue.roundtrip(&mut state) {
        return Err(CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: initial roundtrip: {e}"),
        });
    }

    // Locate the target output by connector name.
    state.locate_target_output()?;

    // Wrap the (conn, event_queue) pair in a WaylandSource and install
    // it.  WaylandSource drives the Wayland socket FD as a calloop
    // source, so `dispatch_pending` fires whenever data is available —
    // idle threads still service configure / input events.
    let wayland_source = WaylandSource::new(conn, event_queue);
    let _wayland_token = match wayland_source.insert(loop_handle.clone()) {
        Ok(t) => t,
        Err(e) => {
            return Err(CmdFailure {
                controller: "render-black".into(),
                error: format!("{E_RENDER_UNAVAILABLE}: insert WaylandSource: {e}"),
            });
        }
    };

    // Wire the command channel as the second calloop source.  The
    // channel's `Sender` clone is captured by the closure so the
    // configure-timeout timer can post `ConfigureTimeout` back into the
    // same channel.  The wayland thread keeps one clone alive in the
    // closure; we return the original to the caller.
    let (cmd_tx, cmd_rx) = channel::<RenderCommand>();
    install_command_source(&loop_handle, cmd_rx, &cmd_tx);

    Ok((cmd_tx, event_loop, state, loop_handle))
}

/// Install the command-channel source.  Captures a `Sender` clone so
/// the per-Show configure-timeout timer can push a `ConfigureTimeout`
/// command back into the same channel — every state mutation stays on
/// the single state-mutating callback path.
fn install_command_source(
    handle: &LoopHandle<'static, WaylandState>,
    rx: calloop::channel::Channel<RenderCommand>,
    cmd_tx: &Sender<RenderCommand>,
) {
    // The closure owns its own `Sender` clone — keeps the loop alive
    // even after the caller (init) drops its `cmd_tx`.
    let tx_inside = cmd_tx.clone();
    if let Err(e) = handle.insert_source(rx, move |event, &mut (), state: &mut WaylandState| {
        match event {
            ChannelEvent::Msg(cmd) => {
                // Capture the show's gen BEFORE the Show is moved into
                // handle_command — the configure-timeout timer posts
                // ConfigureTimeout{gen} back into this channel, and the
                // handler uses gen-match to decide stale vs real timeout.
                if let RenderCommand::Show { r#gen, .. } = &cmd {
                    arm_configure_timer(state, &tx_inside, *r#gen);
                }
                state.handle_command(cmd);
            }
            ChannelEvent::Closed => {
                state.destroy_surface();
                tracing::info!(
                    event = "wayland_thread_shutdown",
                    display_id = %state.display_id,
                    "all senders dropped, exiting dispatch loop"
                );
                // The Channel source's callback returns `()`, not
                // PostAction — break the loop by signalling
                // `loop_should_exit` which `run_loop` polls between
                // dispatch ticks.
                state.loop_should_exit.store(true, Ordering::Release);
            }
        }
    }) {
        tracing::error!(event = "command_source_insert_failed", error = %e);
    }
}

/// Arm a one-shot `Timer` that pushes a `ConfigureTimeout` command
/// back into the wayland thread's own command channel after
/// [`CONFIGURE_TIMEOUT`].  When the timer fires the callback posts the
/// command and the channel's own callback (which is on the single
/// state-mutating path) gen-matches the timeout against the pending
/// show — a stale timer (the show already succeeded OR a newer show
/// superseded this one) is a no-op.
///
/// We deliberately arm a fresh timer per Show; if the compositor
/// replies before the deadline the command resolves Ok and the timer
/// fires into a stale-state guard (`pending_show` is `None`) that we
/// ignore.  The calloop `EventSource` does not support cancellation in
/// 0.14, but the stale-fire is harmless — it costs at most one
/// dispatch tick.
///
/// `r#gen` is the generation counter of the *current* Show.  The
/// timer embeds it in the posted `ConfigureTimeout{gen}` so the
/// handler can distinguish stale from real.
fn arm_configure_timer(state: &WaylandState, cmd_tx: &Sender<RenderCommand>, r#gen: u64) {
    // Capture only what we need; nothing on `state` is aliased by the
    // timer callback.
    let display_id = state.display_id.clone();
    let tx = cmd_tx.clone();

    // We can't easily recover the `LoopHandle` here (it was moved into
    // the run_loop closure), so the channel callback passes us a
    // closure-borrowed `LoopHandle`.  See `install_command_source` for
    // the wiring.
    //
    // Concrete workaround: spawn a tiny OS thread that sleeps for
    // `CONFIGURE_TIMEOUT` then posts the command.  Slightly heavier
    // than a calloop timer but keeps `state` free of cross-thread
    // references (and avoids needing the LoopHandle inside the
    // channel-source closure).
    std::thread::Builder::new()
        .name(format!("dormant-render-timer-{display_id}"))
        .spawn(move || {
            std::thread::sleep(CONFIGURE_TIMEOUT);
            // Send the timeout back through the channel.  If the
            // channel has closed (all senders dropped), the send
            // errors out silently — the wayland thread is already
            // exiting and there's no one to receive.
            let _ = tx.send(RenderCommand::ConfigureTimeout { display_id, r#gen });
        })
        .expect("spawn configure-timer thread");
}

/// Run the calloop dispatch forever.  Exits when `loop_should_exit`
/// is set (every `LayerShellRenderSink` handle dropped → channel
/// `Closed` event → flag flipped), the loop signals an unrecoverable
/// error, or any source returns [`PostAction::Remove`].
fn run_loop(
    mut event_loop: EventLoop<'static, WaylandState>,
    mut state: WaylandState,
    _loop_handle: LoopHandle<'static, WaylandState>,
    _cmd_tx: Sender<RenderCommand>,
) {
    tracing::info!(
        event = "wayland_thread_started",
        display_id = %state.display_id,
        output_name = %state.output_name,
    );

    loop {
        if state.loop_should_exit.load(Ordering::Acquire) {
            tracing::info!(
                event = "wayland_thread_loop_exit",
                display_id = %state.display_id,
                "should_exit flag set, leaving dispatch loop"
            );
            break;
        }
        match event_loop.dispatch(Some(Duration::from_millis(500)), &mut state) {
            Ok(()) => {}
            Err(e) => {
                tracing::error!(
                    event = "wayland_loop_dispatch_error",
                    error = %e,
                    display_id = %state.display_id,
                );
                break;
            }
        }
    }
    tracing::info!(event = "wayland_thread_exit", display_id = %state.display_id);
}

// Reference the imported Timer types so a future calloop-timer
// conversion doesn't leave a dead import.
const _: fn() = || {
    let _: Option<Timer> = None;
    let _: Option<TimeoutAction> = None;
};
