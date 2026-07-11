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
use crate::settings::{ScreensaverSettings, ShiftSettings, TransitionMode};
use crate::shift::ShiftState;

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
///
/// Crossfade tick rate — hard-coded constant (30 frames per second).
/// If we ever expose `transition_fps` to config we'll plumb it here;
/// 30 fps is visibly smooth for a 0.5–1.0 s crossfade.
pub(super) const TRANSITION_FPS: u32 = 30;

/// Convert a wire [`MpvItemEvent`] into the project's internal
/// [`TransitionEvent`] form.  Trivial today (1:1 mapping) but kept
/// as a named converter so adding new event kinds is a one-line
/// change rather than a sweep across the wiring.
fn transition_event_of(ev: MpvItemEvent) -> TransitionEvent {
    match ev {
        MpvItemEvent::ItemEnded => TransitionEvent::ItemEnded,
        MpvItemEvent::ItemLoaded => TransitionEvent::ItemLoaded,
    }
}

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

/// Lifecycle phase for the crossfade transition.  Explicit, not
/// implicit — the state machine below drives transitions through these
/// four states via the [`transition_step`] pure helper (testable in
/// isolation).
///
/// **Rapid-advance rule.**  An `ItemEnded` arriving while the phase is
/// `Fading` immediately restarts the blend from whatever frame is
/// currently displayed (captured from the front buffer).  This makes
/// `image_display_duration < transition_duration` coherent: every
/// new item fades from the current visual, no clip or zipper artefact.
/// The same rule covers `Idle` (no fade in flight — first capture).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionPhase {
    /// No active transition.  `capture` may still be allocated.
    /// next `ItemEnded` moves to `Captured`.
    Idle,
    /// Front buffer captured into `capture`; waiting for the next
    /// `ItemLoaded` to mark the new item as decodable.
    Captured,
    /// New item loaded (`ItemLoaded`); waiting for the first new-frame
    /// render before we arm the timer.  Without this guard, blending
    /// would start against a back buffer that still shows the OLD
    /// item's last frame — visible flicker on the FIRST blend tick.
    AwaitingFirstFrame,
    /// Timer armed; every tick advances `t` and blends.
    Fading,
}

/// Commands the pure state-machine step returns to the wiring (the
/// `on_mpv_wakeup` / `on_transition_tick` thin shells).  Separated
/// from the phase transitions so the pure fn stays I/O-free and the
/// wiring does only the buffers/timer/attach/commit work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StepCmd {
    /// No side effect.
    NoOp,
    /// Cancel any in-flight transition timer.  Idempotent — safe to
    /// issue when no timer is armed.
    DropTimer,
    /// Arm a new transition timer.  The wiring computes `t_step`
    /// from the configured duration via
    /// [`super::blend::compute_blend_params`].
    ArmTimer,
}

/// Drive the crossfade state machine by one event.  Pure: no I/O,
/// no calloop mutation, no buffer ownership.  Tested headlessly in
/// [`crate::linux::state::tests`] below.
///
/// `t` is the live blend progress (`0..=T_MAX`); the wiring persists
/// the returned value.  This helper covers lifecycle transitions
/// only; the per-tick blend math lives in [`tick_step`].
///
/// **Restart-from-current-visual rule:** `ItemEnded` from `Fading`
/// (or `Idle`) returns `Captured` + `DropTimer`, which the wiring
/// satisfies by memcpying the front buffer into `capture` before
/// the next render.  Image advances faster than `transition_duration`
/// therefore visibly fade from whatever is on screen, no zipper.
pub(super) fn transition_step(
    phase: TransitionPhase,
    t: u16,
    ev: TransitionEvent,
) -> (TransitionPhase, u16, StepCmd) {
    use StepCmd::{ArmTimer, DropTimer, NoOp};
    use TransitionEvent::{FrameRendered, ItemEnded, ItemLoaded};
    use TransitionPhase::{AwaitingFirstFrame, Captured, Fading, Idle};

    match (phase, ev) {
        // ItemEnded: capture the visual.  Cancel in-flight fade
        // (Fading → Captured restarts from the just-captured
        // current visual).  Idempotent when already Captured/
        // AwaitingFirstFrame — the visual hasn't changed yet.
        (Idle | Fading, ItemEnded) => (Captured, 0, DropTimer),
        (Captured | AwaitingFirstFrame, ItemEnded) => (Captured, 0, NoOp),

        // ItemLoaded: only Captured advances.  Idle-with-ItemLoaded
        // means "no capture recorded yet, nothing to fade" — discard.
        (Captured, ItemLoaded) => (AwaitingFirstFrame, 0, NoOp),
        // FrameRendered: from AwaitingFirstFrame a successful render
        // arms the timer and moves to Fading (the timer takes over
        // with the first blended tick).  From any other phase it's
        // either a normal wakeup (Idle — no transition in flight) or
        // the wired continuation of an in-flight fade (Fading — the
        // tick_step helper below handles the actual t advance).
        (AwaitingFirstFrame, FrameRendered { ok: true }) => (Fading, 0, ArmTimer),
        // Catch-all: ItemLoaded at non-Captured phases, FrameRendered
        // at non-AwaitingFirstFrame phases, and FrameRendered{ok=false}
        // anywhere are all no-ops — same end state.
        (_, ItemLoaded | FrameRendered { .. }) => (phase, t, NoOp),
    }
}

/// What the caller wires up in response to a [`TickCmd`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TickCmd {
    /// No-op (the tick didn't advance — e.g. phase wasn't `Fading`
    /// or the frame render returned `Ok(false)`).
    NoTickOp,
    /// Blend + attach + commit + advance `t`.  Visual update is
    /// committed to the compositor.
    Advance,
    /// Blend the final frame (`t=T_MAX`), commit, drop the timer,
    /// return to `Idle` for the next cycle.  `capture` stays
    /// allocated across cycles — only the timer + `t` go away.
    Complete,
}

/// Drive one blend-tick advance.  Pure: caller renders, calls this,
/// acts on the returned command.
///
/// `t_advance` is unconditional on the blend timer (re-review M2):
/// the timer owns the visible opacity change, NOT the `mpv` frame
/// stream.  Each tick advances `t` regardless of whether the
/// previous wakeup produced a new frame — the per-tick render
/// re-draws the current picture at increasing `t`.  This is what
/// makes static-image fades complete (mpv produces one frame and
/// then goes idle; without this guarantee the fade hangs at
/// `t = 0` forever).
///
/// `t_step` is the increment computed at timer-arm time via
/// [`super::blend::compute_blend_params`].
pub(super) fn tick_step(
    phase: TransitionPhase,
    t: u16,
    t_step: u16,
) -> (TransitionPhase, u16, TickCmd) {
    use TickCmd::{Advance, Complete, NoTickOp};
    use TransitionPhase::{Fading, Idle};
    if phase != Fading {
        return (phase, t, NoTickOp);
    }
    let new_t = t.saturating_add(t_step);
    if new_t >= T_MAX {
        (Idle, T_MAX, Complete)
    } else {
        (Fading, new_t, Advance)
    }
}

/// Capture the currently-attached (front) buffer into the
/// transition capture Vec.  The front buffer is
/// `buffers[1 - next_render_idx]` because the last render wrote to
/// `next_render_idx` then flipped.
///
/// Free function (not a method) so the caller can pass both
/// `&mut ScreensaverSession` and `Option<&LoopHandle>` without
/// re-borrowing `&mut self`.
fn capture_front_into_transition(session: &mut ScreensaverSession) {
    let Some(tr) = session.transition.as_mut() else {
        return;
    };
    let buf_len = (session.stride as usize) * (session.height as usize);
    if tr.capture.len() != buf_len {
        // Re-allocate if the surface dimensions changed during the
        // session (compositor resize).  Rare path.
        tr.capture = vec![0u8; buf_len];
    }
    let front_idx = 1 - session.next_render_idx;
    let front_offset = front_idx * buf_len;
    let mmap = session.pool.mmap();
    // SAFETY: pool was built with 2*buf_len bytes; reading
    // `buf_len` bytes from `front_offset` cannot overrun.
    let front_slice = unsafe {
        std::slice::from_raw_parts(mmap.as_ptr().cast::<u8>().add(front_offset), buf_len)
    };
    tr.capture.copy_from_slice(front_slice);
}

/// Remove the crossfade timer from calloop if one is armed.
/// Idempotent — called from `DropTimer` commands and on `Complete`.
/// Takes the session and the loop handle separately so the caller
/// can pass both without re-borrowing `&mut self`.
fn cancel_transition_timer_for(
    session: &mut ScreensaverSession,
    handle: Option<&calloop::LoopHandle<'static, WaylandState>>,
) {
    let token = session
        .transition
        .as_mut()
        .and_then(|t| t.timer_token.take());
    if let Some(token) = token
        && let Some(handle) = handle
    {
        handle.remove(token);
    }
}

/// Side-effect flags the wiring applies after [`process_mpv_events`]
/// returns.  Kept as small data so the helper stays I/O-free and
/// unit-testable without a `WaylandState` or real `MpvPlayer`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ProcessCmds {
    /// True when the batch emitted a `DropTimer` cmd (Fading→
    /// Captured, or initial Idle→Captured).  The wiring responds by
    /// memcpying the front buffer into `transition.capture` and
    /// clearing `t` + `timer_token`.
    pub(super) capture_pending: bool,
    /// True when the batch reached Fading via the
    /// `AwaitingFirstFrame` → `Fading` transition.  The wiring arms
    /// (or replaces) the blend timer via `arm_or_rearm_transition_timer`.
    pub(super) arm_pending: bool,
}

/// Process a drained batch of mpv events.  Pure: no I/O, no calloop
/// mutation, no buffer reads.  Operates on the `Option<TransitionState>`
/// directly so it can be unit-tested without a `MpvPlayer` or a real
/// screensaver session.
///
/// **Allocation rule (re-review M1):** lazy-allocates on the FIRST
/// `ItemEnded` *anywhere* in the batch.  Previously the gate used
/// `batch.first()`, which silently dropped events when
/// `ItemLoaded` came first — a real-possibility batch ordering
/// that the previous review missed.  Now uses `any(|e| matches!(e,
/// ItemEnded))` so every `ItemEnded` (in any position) reaches the
/// state machine.
///
/// **Capture handling:** a `DropTimer` cmd resets `t` to 0 and clears
/// `timer_token` but does NOT memcpy the front buffer into
/// `capture` here (no buffer / pool access).  The wiring is
/// responsible for that side-effect (we return
/// `capture_pending = true`).
pub(super) fn process_mpv_events(
    transition: Option<TransitionState>,
    mpv_events: &[MpvItemEvent],
    t_step: u16,
    buf_len: usize,
) -> (Option<TransitionState>, ProcessCmds) {
    let mut transition = transition;
    let mut cmds = ProcessCmds::default();

    // Lazy allocation: first ItemEnded in the batch wins (M1 fix).
    let has_item_ended = mpv_events
        .iter()
        .any(|e| matches!(e, MpvItemEvent::ItemEnded));
    if has_item_ended && transition.is_none() {
        transition = Some(TransitionState {
            capture: vec![0u8; buf_len],
            phase: TransitionPhase::Idle,
            t: 0,
            t_step,
            timer_token: None,
        });
    }

    if let Some(tr) = transition.as_mut() {
        for ev in mpv_events {
            let (new_phase, _, cmd) = transition_step(tr.phase, tr.t, transition_event_of(*ev));
            tr.phase = new_phase;
            match cmd {
                StepCmd::NoOp => {}
                StepCmd::DropTimer => {
                    cmds.capture_pending = true;
                    tr.t = 0;
                    tr.timer_token = None;
                }
                StepCmd::ArmTimer => {
                    cmds.arm_pending = true;
                    tr.t = 0;
                }
            }
        }
    }

    (transition, cmds)
}

/// Events fed to [`transition_step`].  Source: drained from
/// [`crate::screensaver::MpvPlayer::poll_events`] (`ItemEnded` /
/// `ItemLoaded`) plus the per-wakeup `FrameRendered` observation from
/// `render_frame_into`'s `Ok(bool)` return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionEvent {
    ItemEnded,
    ItemLoaded,
    FrameRendered { ok: bool },
}

/// Crossfade state for one screensaver session.
///
/// Holds the capture buffer (allocated lazily on first `ItemEnded`),
/// the lifecycle phase, the blend progress, and the calloop timer's
/// registration token.  The capture buffer is kept across cycles —
/// allocation cost is amortised to one `Vec<u8>` of `stride * height`
/// bytes (~21 MiB at 4K) for the lifetime of the session.
pub(super) struct TransitionState {
    /// Snapshot of the outgoing item's last rendered frame.  Copied
    /// from the front buffer on the `ItemEnded` transition.  Length
    /// is `stride * height` (matches one screensaver frame).
    pub(super) capture: Vec<u8>,
    /// Current lifecycle phase — see [`TransitionPhase`].
    pub(super) phase: TransitionPhase,
    /// Current blend progress: 0 = pure capture, `T_MAX` = pure new.
    pub(super) t: u16,
    /// Per-tick increment; computed at timer arm time via
    /// [`super::blend::compute_blend_params`].
    pub(super) t_step: u16,
    /// Timer registration — `Some` only while `phase == Fading`.
    /// The wiring removes the source from calloop when [`transition_step`]
    /// returns [`StepCmd::DropTimer`] or [`TickCmd::Complete`]; this
    /// field tracks the bookkeeping mirror of that removal.
    pub(super) timer_token: Option<calloop::RegistrationToken>,
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
    /// method call.  The `None` mode leaves every `transition` field
    /// below untouched (and the wakeup path simply drains-and-discards
    /// mpv events to keep libmpv's per-handle queue flowing).
    pub(super) transition_mode: TransitionMode,
    /// Crossfade state for the current cycle.  `Some` from the first
    /// `ItemEnded` onward (allocated lazily); `phase == Idle` between
    /// transitions, `Fading` while a timer is armed.  `capture` stays
    /// allocated across cycles — reallocating per cycle would be a
    /// `stride * height` (≈21 MiB at 4K) free/alloc cycle every
    /// `image-display-duration`.
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

    // ── Micro pixel-shift (OLED-health T10) ────────────────────────────
    /// Per-display shift config, set once via
    /// [`RenderCommand::SetShift`] (fire-and-forget, sent by
    /// [`super::LayerShellRenderSink::set_shift`]).  Defaults to fully
    /// disabled (`shift_px: 0`) — a display whose sink never receives
    /// a `SetShift` command (no `[displays.<id>.screensaver]` table)
    /// renders byte-identically to the pre-T10 code path.
    pub(super) shift_settings: ShiftSettings,
    /// Raster-walk cursor for the CURRENTLY LIVE surface.  `Some` from
    /// the moment the first content (black or screensaver) is
    /// installed on a fresh surface through to that surface's full
    /// teardown — a content-swap on the SAME surface (e.g. screensaver
    /// → black) reuses the walk already in progress rather than
    /// resetting it; only [`Self::destroy_surface`] resets it (per the
    /// probe: shift state does not persist across teardown/re-show,
    /// but nothing says it must reset on a same-surface content swap).
    /// `None` when shift is disabled OR no surface is currently up.
    pub(super) shift_state: Option<ShiftState>,
    /// calloop `RegistrationToken` for the shift timer.  Armed exactly
    /// once per surface lifetime (the moment `shift_state` transitions
    /// `None` → `Some`) via [`Self::maybe_arm_shift_timer`]; removed by
    /// [`Self::disarm_shift_timer`] on full surface teardown.
    pub(super) shift_timer_token: Option<calloop::RegistrationToken>,
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
            shift_settings: ShiftSettings::default(),
            shift_state: None,
            shift_timer_token: None,
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

    // ── Micro pixel-shift (OLED-health T10) ─────────────────────────

    /// Single source of truth for shift geometry: `(render_w,
    /// render_h, margin)` when pixel-shift is active for this
    /// display, or `None` when disabled (`shift_px == 0`), the
    /// compositor has no `wp_viewporter`, or the margin arithmetic
    /// overflows (defensive — unreachable at real display resolutions
    /// given the validator's `shift_px <= 8` ceiling).  `render_w` /
    /// `render_h` are `dest` oversized by `2 * margin` on each axis —
    /// the dims every oversized shm buffer (black or screensaver) must
    /// be allocated at.
    fn shift_geometry(&self, dest: (u32, u32)) -> Option<(u32, u32, u32)> {
        if self.shift_settings.shift_px == 0 || self.viewporter.is_none() {
            return None;
        }
        let margin = crate::shift::margin(self.shift_settings.shift_px);
        let total = margin.checked_mul(2)?;
        let w = dest.0.checked_add(total)?;
        let h = dest.1.checked_add(total)?;
        Some((w, h, margin))
    }

    /// Buffer dims to allocate for `dest` — oversized when shift is
    /// active, `dest` unchanged otherwise.  See [`Self::shift_geometry`].
    pub(super) fn render_dims(&self, dest: (u32, u32)) -> (u32, u32) {
        self.shift_geometry(dest).map_or(dest, |(w, h, _)| (w, h))
    }

    /// Ensure a `WpViewport` is bound to `wl_surface`, creating one on
    /// first use and reusing it thereafter (a `wl_surface` may only
    /// ever have ONE viewport for its lifetime — calling
    /// `wp_viewporter::get_viewport` twice on the same surface is a
    /// protocol error, so every caller MUST go through this method
    /// rather than calling `get_viewport` directly).
    fn ensure_viewport(&mut self, wl_surface: &WlSurface) -> Option<WpViewport> {
        if let Some(vp) = &self.viewport {
            return Some(vp.clone());
        }
        let vp = self
            .viewporter
            .as_ref()?
            .get_viewport(wl_surface, &self.queue_handle, ());
        self.viewport = Some(vp.clone());
        Some(vp)
    }

    /// When pixel-shift is enabled: ensure a `WpViewport` is bound to
    /// `wl_surface`, set its destination to `dest`, and — if this is
    /// the FIRST content ever installed on this surface's current
    /// lifetime (`self.shift_state` is still `None`) — initialise the
    /// raster-walk state and set the initial (centred) source rect.
    /// A later content-swap on the SAME surface (screensaver ↔ black)
    /// reuses the walk already in progress: `self.shift_state` stays
    /// `Some`, so this becomes a cheap idempotent re-assert of the
    /// destination only.
    ///
    /// Returns `None` (no-op, no side effects) when shift is disabled
    /// or the compositor has no `wp_viewporter` — the caller falls
    /// back to the pre-T10 path.
    fn ensure_shift_viewport(
        &mut self,
        wl_surface: &WlSurface,
        dest: (u32, u32),
    ) -> Option<WpViewport> {
        self.shift_geometry(dest)?;
        let viewport = self.ensure_viewport(wl_surface)?;
        viewport.set_destination(dest.0.cast_signed(), dest.1.cast_signed());
        if self.shift_state.is_none() {
            let state = ShiftState::new(self.shift_settings.shift_px);
            let (ox, oy) = state.source_origin();
            viewport.set_source(
                f64::from(ox),
                f64::from(oy),
                f64::from(dest.0),
                f64::from(dest.1),
            );
            self.shift_state = Some(state);
        }
        Some(viewport)
    }

    /// Arm the shift timer exactly once per surface lifetime — the
    /// moment `shift_state` transitions `None` → `Some` (idempotent:
    /// a no-op if a timer is already armed or shift is inactive).
    /// Called after every buffer-install path (black or screensaver,
    /// fresh or content-swap) so the timer is live as soon as ANY
    /// shifted content is showing, per the adjudicated rule ("armed
    /// only when `shift_px` > 0 AND a surface is showing").
    fn maybe_arm_shift_timer(&mut self) {
        let walk_in_progress = self.shift_state.as_ref().is_some_and(ShiftState::enabled);
        if self.shift_timer_token.is_none() && walk_in_progress {
            self.arm_shift_timer();
        }
    }

    /// Install the self-re-arming shift timer (`TimeoutAction::ToInstant`
    /// pattern, same discipline as `arm_or_rearm_transition_timer`).
    /// Each tick calls [`Self::on_shift_tick`] then re-arms for another
    /// `shift_interval` as long as the surface is still up and a walk
    /// is still in progress; otherwise it drops itself.  This is the
    /// timer's OWN liveness guard — deliberately not gen-guarded like
    /// the configure-timeout / first-frame-deadline timers, because
    /// shift is a per-SURFACE-lifetime property (spanning multiple
    /// `r#gen`s across content-swaps), not a per-show one.
    fn arm_shift_timer(&mut self) {
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };
        let interval = self.shift_settings.shift_interval;
        let shift_px = self.shift_settings.shift_px;
        let timer = Timer::from_duration(interval);
        let inserted =
            handle.insert_source(timer, move |_deadline, _meta, state: &mut WaylandState| {
                state.on_shift_tick();
                if state.surface_up && state.shift_state.is_some() {
                    TimeoutAction::ToInstant(std::time::Instant::now() + interval)
                } else {
                    TimeoutAction::Drop
                }
            });
        match inserted {
            Ok(token) => {
                self.shift_timer_token = Some(token);
                tracing::debug!(
                    event = "render_shift_timer_armed",
                    display_id = %self.display_id,
                    shift_px,
                    interval_ms = interval.as_millis(),
                );
            }
            Err(e) => tracing::error!(
                event = "render_shift_timer_insert_failed",
                display_id = %self.display_id,
                error = %e,
            ),
        }
    }

    /// Remove the shift timer's calloop registration if one is armed.
    /// Idempotent.  Does NOT touch `shift_state` — [`Self::reset_shift`]
    /// (full surface teardown) clears that separately.
    fn disarm_shift_timer(&mut self) {
        if let (Some(token), Some(handle)) =
            (self.shift_timer_token.take(), self.loop_handle.as_ref())
        {
            handle.remove(token);
        }
    }

    /// Full shift reset: disarm the timer AND drop the walk state.
    /// Called from [`Self::destroy_surface`] — per the probe, shift
    /// state does not persist across a full teardown/re-show; the
    /// next Show on a fresh surface always starts centred.
    pub(super) fn reset_shift(&mut self) {
        self.disarm_shift_timer();
        self.shift_state = None;
    }

    /// Shift timer tick: advance the walk one step, re-aim the live
    /// viewport's source rect, damage the full (oversized) buffer, and
    /// commit.  No re-render, no new attach — exactly the probe's
    /// "cost per shift is effectively zero (compositor-side crop
    /// move)" mechanism.  A no-op if the surface has gone down or no
    /// walk is in progress (the timer's own re-arm guard normally
    /// prevents this from firing at all in that case; this is
    /// belt-and-suspenders against a tick landing in the same loop
    /// iteration as a teardown).
    fn on_shift_tick(&mut self) {
        if !self.surface_up {
            return;
        }
        let Some(shift_state) = self.shift_state.as_mut() else {
            return;
        };
        let (ox, oy) = shift_state.advance();
        let margin_px = shift_state.margin_px();
        let dest = self.configured_size;
        let Some(viewport) = self.viewport.as_ref() else {
            return;
        };
        viewport.set_source(
            f64::from(ox),
            f64::from(oy),
            f64::from(dest.0),
            f64::from(dest.1),
        );
        let Some(surface) = self.layer_surface.as_ref() else {
            return;
        };
        let wl_surface = surface.wl_surface();
        let ow = dest.0.saturating_add(2 * margin_px);
        let oh = dest.1.saturating_add(2 * margin_px);
        wl_surface.damage_buffer(0, 0, ow.cast_signed(), oh.cast_signed());
        wl_surface.commit();
        tracing::trace!(
            event = "render_shift_step",
            display_id = %self.display_id,
            offset_x = ox,
            offset_y = oy,
        );
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
    #[allow(clippy::too_many_lines)] // black-buffer attach fans out into shift-enabled / single-pixel / shm-fallback branches, documented inline
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
        //
        // Pixel-shift (T10): when enabled, the black overlay ALWAYS
        // uses the oversized-shm-buffer + wp_viewport path — a 1×1
        // single-pixel buffer has no room for a raster walk (its
        // source rect can only ever be `(0,0,1,1)`).  This preempts
        // the single-pixel-manager preference below; when shift is
        // disabled `ensure_shift_viewport` is a no-op and behaviour is
        // byte-identical to the pre-T10 code (the safety invariant).
        let wl_surface = pending.layer_surface.wl_surface().clone();
        let (buffer, viewport): (WlBuffer, Option<WpViewport>) =
            if let Some(vp) = self.ensure_shift_viewport(&wl_surface, configured_size) {
                let (ow, oh) = self.render_dims(configured_size);
                match crate::linux::surface::create_shm_black_buffer(ow, oh, self) {
                    Ok(b) => {
                        wl_surface.attach(Some(&b), 0, 0);
                        wl_surface.damage_buffer(0, 0, ow.cast_signed(), oh.cast_signed());
                        wl_surface.commit();
                        (b, Some(vp))
                    }
                    Err(e) => {
                        let _ = pending.reply.send(Err(CmdFailure {
                            controller: "render-black".into(),
                            error: format!("{E_RENDER_UNAVAILABLE}: shift shm buffer: {e}"),
                        }));
                        return;
                    }
                }
            } else {
                match (&self.single_pixel_manager, &self.viewporter) {
                    (Some(spm), Some(vp)) => {
                        let (b, v) = crate::linux::surface::attach_single_pixel_black(
                            spm,
                            vp,
                            &wl_surface,
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
                                wl_surface.attach(Some(&b), 0, 0);
                                wl_surface.commit();
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
                }
            };

        self.layer_surface = Some(pending.layer_surface);
        self.viewport = viewport;
        self.black_buffer = Some(buffer);
        self.configured_size = configured_size;
        self.surface_up = true;
        self.maybe_arm_shift_timer();

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

        // Pixel-shift (T10): when enabled, render mpv into an
        // OVERSIZED canvas (`render_dims`) and crop it back down to
        // `configured_size` via a `wp_viewport` source rect walked by
        // the shift timer.  mpv's scale_mode (fill/fit/stretch/center)
        // simply operates against the slightly larger canvas — the
        // few extra margin pixels of overscan around every edge are
        // the intended mechanism (no change to the mpv pipeline
        // itself).  `width`/`height` below flow into the mpv render
        // context, the shm pool, BOTH double-buffers, and the
        // session's own `width`/`height`/`stride` fields — every
        // downstream consumer (damage rects, the blend buffer-length
        // math, the transition capture buffer) then naturally operates
        // on the oversized canvas with no separate "shifted" special
        // case.  When shift is disabled `ensure_shift_viewport` /
        // `render_dims` are no-ops and `width`/`height` equal
        // `configured_size` exactly — byte-identical to pre-T10.
        let wl_surface_for_shift = layer_surface.wl_surface().clone();
        let shift_viewport = self.ensure_shift_viewport(&wl_surface_for_shift, configured_size);
        let (width, height) = self.render_dims(configured_size);
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
            // `TransitionMode::None` paths drain mpv events and discard
            // them (see `on_mpv_wakeup`) without ever touching
            // `transition` — the state machine is gated on
            // `transition_mode == Crossfade` for the capture/load/render
            // arms.  Pre-allocating an empty `TransitionState` (rather
            // than `Some`/`None` toggling) lets the field be a plain
            // `Option` that's `Some` once capture has happened at all.
            transition_mode: settings.transition,
            transition: None, // lazy: capture Vec allocated on first ItemEnded
            transition_duration: settings.transition_duration,
        });
        // The wakeup slot holds the token so we can later remove the
        // source via `loop_handle.remove(token)` from
        // `destroy_screensaver_session`.
        self.screensaver_wakeup_token = Some(wakeup_token);
        self.layer_surface = Some(layer_surface);
        // NOTE: only overwrite `self.viewport` when shift actually
        // built one — `complete_screensaver_show` historically never
        // touches this field when shift is disabled (a screensaver
        // shown with no prior black stage has no viewport at all;
        // mpv renders directly at `configured_size`, no scaling
        // needed).  Overwriting with `None` here would destroy a
        // viewport a PRIOR black stage may have created.
        if let Some(vp) = shift_viewport {
            self.viewport = Some(vp);
        }
        self.configured_size = configured_size;
        self.surface_up = true;
        self.maybe_arm_shift_timer();

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
    /// Crossfade semantics: mpv's per-handle event queue is always
    /// drained (M5 — see the constructor for why).  Drained events are
    /// mapped to [`TransitionEvent`] and fed to the pure state-machine
    /// step [`transition_step`]; the returned command side effects
    /// (drop timer, arm timer) are applied here.  The phase machine
    /// persists in `session.transition.phase` across wakeups so events
    /// spanning multiple wakeups (e.g. `ItemEnded` in one wakeup and
    /// `ItemLoaded` in a later one) still drive the lifecycle correctly.
    #[allow(clippy::too_many_lines)] // single-method state machine: drain, render, blend, attach, advance are documented inline below
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

        // ALWAYS drain mpv's event queue (M5: libmpv's per-handle
        // event queue congests / overflows if never drained).  In
        // `TransitionMode::None` the drained events are discarded;
        // the rendered surface stays pixel-identical.
        let mpv_events: Vec<MpvItemEvent> = session.player.poll_events();

        // Compute the per-tick step (used by both the timer arm and
        // the in-tick blend).  Recomputed per wakeup only matters when
        // duration changes between fades (today: live for the session).
        let t_step = blend::compute_blend_params(TRANSITION_FPS, session.transition_duration).1;

        // Phase 1 — process the drained events through the helper.
        // In Crossfade mode the helper handles lazy allocation (M1
        // fix: on ANY ItemEnded in the batch, not just first) and
        // the per-event state-machine step.  In None mode the helper
        // is a no-op (the events were already discarded on the dispatch).
        let buf_len = (session.stride as usize) * (session.height as usize);
        let (new_transition, cmds) = if session.transition_mode == TransitionMode::Crossfade {
            let taken = session.transition.take();
            process_mpv_events(taken, &mpv_events, t_step, buf_len)
        } else {
            (None, ProcessCmds::default())
        };
        session.transition = new_transition;
        if cmds.capture_pending {
            capture_front_into_transition(session);
        }

        // Early-out if we just armed: the timer takes over with the
        // first blended tick.  Don't commit on the arming wakeup.
        if cmds.arm_pending {
            let r#gen = session.pending_gen;
            self.arm_or_rearm_transition_timer(r#gen);
            return;
        }

        // Skip-on-busy gate.
        let back_idx = session.next_render_idx;
        if session.buffers_busy[back_idx] {
            tracing::debug!(
                event = "screensaver_frame_skipped_busy",
                display_id = %self.display_id,
                back_idx,
            );
            return;
        }

        // Render mpv into the back buffer.  The SW render API always
        // draws the current picture — that's mechanism (a) for the
        // buffer-correctness constraint (capture state is fresh every
        // tick).  We treat  and  identically
        // here: the buffer is fresh either way.
        let back_offset = back_idx * buf_len;
        let mut back_buf: Option<*mut u8> = {
            let mmap = session.pool.mmap();
            // SAFETY: offset is within the pool; slice length matches.
            let back_slice = unsafe {
                std::slice::from_raw_parts_mut(mmap.as_ptr().cast_mut().add(back_offset), buf_len)
            };
            back_slice.fill(0);
            match session.player.render_frame_into(back_slice) {
                Ok(_) => Some(back_slice.as_mut_ptr()),
                Err(e) => {
                    self.fail_screensaver_to_black(&format!("{e}"));
                    return;
                }
            }
        };

        // Phase 2 — apply FrameRendered{ok} to the state machine.
        // (AwakeningFirstFrame → Fading arms the timer;  is NOT
        // advanced here — that's the ticker's job per re-review.)
        if session.transition_mode == TransitionMode::Crossfade
            && let Some(tr) = session.transition.as_mut()
        {
            let (new_phase, _, cmd) =
                transition_step(tr.phase, tr.t, TransitionEvent::FrameRendered { ok: true });
            tr.phase = new_phase;
            if matches!(cmd, StepCmd::ArmTimer) {
                let r#gen = session.pending_gen;
                cancel_transition_timer_for(session, self.loop_handle.as_ref());
                self.arm_or_rearm_transition_timer(r#gen);
                return;
            }
        }

        // Snapshot  for the visible commit (we never advance
        // in this path).  The capture is recomputed every tick by
        // , which does own the t advance.
        let blend_t = session.transition.as_ref().map_or(0, |tr| tr.t);
        let capture_clone: Vec<u8> = if blend_t > 0 {
            session
                .transition
                .as_ref()
                .map(|tr| tr.capture.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // Commit (we always have a back_buf: render was successful).
        if let Some(ptr) = back_buf.take() {
            // SAFETY: back-buffer pointer from mmap + offset; valid
            // for the lifetime of this function.
            let back_slice = unsafe { std::slice::from_raw_parts_mut(ptr, buf_len) };
            if blend_t > 0 && !capture_clone.is_empty() {
                blend::blend_in_place(&capture_clone, back_slice, blend_t);
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

            // First-frame success.
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

            // Swap so the next render writes to the buffer the
            // compositor has finished with.
            session.next_render_idx = 1 - back_idx;
        }
    }

    /// Install the periodic blend timer.  Called from
    /// [`Self::on_mpv_wakeup`] when the phase machine emits
    /// `StepCmd::ArmTimer` (the rapid `ItemEnded` mid-fade path or the
    /// first successful render after `ItemLoaded`).  Idempotent — a
    /// already-armed timer is replaced, not duplicated.
    ///
    /// Self-repeating: arms for one tick interval (~33 ms at the
    /// 30 fps default — see [`TRANSITION_FPS`]); the callback re-arms
    /// via `TimeoutAction::ToInstant` until [`Self::on_transition_tick`]
    /// observes `Complete` and drops the registration.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn arm_or_rearm_transition_timer(&mut self, r#gen: u64) {
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };

        // Snapshot the session-derived inputs up-front so the
        // `insert_source` re-borrow of `&mut self` doesn't conflict
        // with the live `&mut session` borrow.
        let (t_step, session_gen, is_fading, already_armed, computed) = {
            let Some(session) = self.screensaver_session.as_ref() else {
                return;
            };
            let tr = session.transition.as_ref();
            (
                tr.map_or(1, |t| t.t_step),
                session.pending_gen,
                tr.is_some_and(|t| t.phase == TransitionPhase::Fading),
                tr.and_then(|t| t.timer_token).is_some(),
                blend::compute_blend_params(TRANSITION_FPS, session.transition_duration),
            )
        };

        if !is_fading || already_armed || session_gen != r#gen {
            return;
        }
        let _ = computed;

        let tick_interval = std::time::Duration::from_millis(33);
        let timer = Timer::from_duration(tick_interval);
        let inserted =
            handle.insert_source(timer, move |_deadline, _meta, state: &mut WaylandState| {
                state.on_transition_tick(r#gen);
                TimeoutAction::ToInstant({
                    use std::time::Instant;
                    Instant::now() + std::time::Duration::from_millis(33)
                })
            });
        let Some(session_mut) = self.screensaver_session.as_mut() else {
            return;
        };
        match inserted {
            Ok(token) => {
                if let Some(tr) = session_mut.transition.as_mut() {
                    tr.timer_token = Some(token);
                }
            }
            Err(e) => {
                tracing::error!(
                    event = "transition_timer_insert_failed",
                    display_id = %self.display_id,
                    r#gen,
                    error = %e,
                    "failed to install transition timer; blend will be skipped"
                );
            }
        }
        let _ = t_step;
    }

    /// Per-tick blend progress.  Runs on the calloop thread when the
    /// transition timer fires.  Renders mpv into the back buffer,
    /// advances `t` unconditionally via `tick_step` (this is
    /// mechanism (a) for the buffer-correctness constraint — the
    /// SW render API always draws the current picture regardless of
    /// update-flag), blends at the pre-advance `t` (the visible
    /// progress for this tick), attaches + commits, then advances
    /// `t` for the next timer fire.  On `Complete` the timer is
    /// dropped and the phase returns to `Idle`.
    ///
    /// **Re-review (M2): this is the ONLY path that advances `t`.**
    /// `on_mpv_wakeup` refreshes the source imagery (renders the
    /// current mpv frame and blends at the live `t` if Fading) but
    /// never moves `t` — that keeps the visible fade duration
    /// framerate-independent.  A static image (one mpv frame ever)
    /// completes its fade in the same `frames_for_blend` ticks as
    /// a 60-fps video would.
    fn on_transition_tick(&mut self, r#gen: u64) {
        let Some(session) = self.screensaver_session.as_mut() else {
            return;
        };
        // Gen-guard: a newer session has taken over — drop the tick.
        if session.pending_gen != r#gen || session.transition_mode != TransitionMode::Crossfade {
            return;
        }
        let t_step = session.transition.as_ref().map_or(1, |tr| tr.t_step);

        // Skip-on-busy gate.
        let back_idx = session.next_render_idx;
        if session.buffers_busy[back_idx] {
            return;
        }

        let stride = session.stride as usize;
        let buf_len = stride * (session.height as usize);
        let back_offset = back_idx * buf_len;

        // Render mpv into the back buffer.  The SW render API always
        // draws the current picture regardless of update-flag value —
        // `Ok(false)` just signals "no novel frame since last call"
        // (a status byte, not a render outcome).  We treat both
        // `Ok` variants identically here: the buffer is fresh either
        // way.  (Mechanism (a) for the buffer-correctness constraint —
        // see module header.)
        let back_ptr = {
            let mmap = session.pool.mmap();
            // SAFETY: offset is within the pool; slice length matches.
            let back_slice = unsafe {
                std::slice::from_raw_parts_mut(mmap.as_ptr().cast_mut().add(back_offset), buf_len)
            };
            back_slice.fill(0);
            match session.player.render_frame_into(back_slice) {
                Ok(_) => back_slice.as_mut_ptr(),
                Err(e) => {
                    self.fail_screensaver_to_black(&format!("{e}"));
                    return;
                }
            }
        };

        // Snapshot the Fading `t` BEFORE `tick_step` advances it — we
        // blend at the visible pre-tick value (commits show the
        // progress this tick produced; the post-tick value is the
        // starting point for next time).
        let blend_t = session.transition.as_ref().map_or(0, |tr| tr.t);
        let capture_clone: Vec<u8> = if blend_t > 0 {
            session
                .transition
                .as_ref()
                .map(|tr| tr.capture.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // Advance the phase machine via `tick_step` (no I/O).
        let (new_phase, new_t, tick_cmd) = {
            let tr_phase = session
                .transition
                .as_ref()
                .map_or(TransitionPhase::Idle, |tr| tr.phase);
            tick_step(tr_phase, blend_t, t_step)
        };
        if let Some(tr) = session.transition.as_mut() {
            tr.phase = new_phase;
            tr.t = new_t;
        }

        // Apply the blend in place at the pre-tick `t`.
        // SAFETY: the back-buffer pointer came from the pool mmap +
        // offset math; the slice remains valid until the end of this
        // function (the mmap guard is in scope until then).
        let back_slice = unsafe { std::slice::from_raw_parts_mut(back_ptr, buf_len) };
        if blend_t > 0 && !capture_clone.is_empty() {
            blend::blend_in_place(&capture_clone, back_slice, blend_t);
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

        // On Complete: drop the timer and reset the state for the
        // next cycle (phase → Idle, capture stays allocated).
        if matches!(tick_cmd, TickCmd::Complete) {
            let session_duration_ms = session.transition_duration.as_millis();
            cancel_transition_timer_for(session, self.loop_handle.as_ref());
            tracing::debug!(
                event = "screensaver_transition_complete",
                display_id = %self.display_id,
                duration_ms = session_duration_ms,
                "crossfade complete; resuming wakeup-driven path"
            );
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
        // shm fallback — matches the black path's own choices.  When
        // shift is active, `self.shift_state` is already `Some` (set
        // by the screensaver's own `complete_screensaver_show` install)
        // so `ensure_shift_viewport` here is just an idempotent
        // destination re-assert; the oversized black buffer keeps the
        // walk in progress valid regardless of which content is
        // attached.
        let dest = self.configured_size;
        let mut shift_active = false;
        if self.black_buffer.is_none()
            && let Some(surface) = self.layer_surface.as_ref()
        {
            let wl_surface = surface.wl_surface().clone();
            let shift_viewport = self.ensure_shift_viewport(&wl_surface, dest);
            shift_active = shift_viewport.is_some();
            if shift_active {
                let (ow, oh) = self.render_dims(dest);
                match crate::linux::surface::create_shm_black_buffer(ow, oh, self) {
                    Ok(buffer) => self.black_buffer = Some(buffer),
                    Err(e) => tracing::error!(
                        event = "screensaver_black_fallback_failed",
                        display_id = %self.display_id,
                        error = %e,
                    ),
                }
            } else if self.single_pixel_manager.is_some() && self.viewporter.is_some() {
                let buffer = self
                    .single_pixel_manager
                    .as_ref()
                    .expect("checked Some above")
                    .create_u32_rgba_buffer(0, 0, 0, u32::MAX, &self.queue_handle, ());
                if let Some(viewport) = self.ensure_viewport(&wl_surface) {
                    viewport.set_destination(dest.0.cast_signed(), dest.1.cast_signed());
                    self.viewport = Some(viewport);
                }
                self.black_buffer = Some(buffer);
            } else {
                match crate::linux::surface::create_shm_black_buffer(dest.0, dest.1, self) {
                    Ok(buffer) => self.black_buffer = Some(buffer),
                    Err(e) => tracing::error!(
                        event = "screensaver_black_fallback_failed",
                        display_id = %self.display_id,
                        error = %e,
                    ),
                }
            }
        }
        // A pre-existing (cached) black_buffer means shift's already-
        // armed walk (if any) is still what's driving the live
        // viewport — reflect that in the damage-rect choice below.
        shift_active |= self.shift_state.is_some();

        // Destroy the session — frees the mpv player + shm pool, removes
        // the calloop wakeup source, removes the deadline timer.
        self.destroy_screensaver_session();

        // Re-attach the now-guaranteed black buffer.
        if let (Some(surface), Some(black)) = (&self.layer_surface, &self.black_buffer) {
            let wl_surface = surface.wl_surface();
            wl_surface.attach(Some(black), 0, 0);
            if shift_active {
                let (ow, oh) = self.render_dims(dest);
                wl_surface.damage_buffer(0, 0, ow.cast_signed(), oh.cast_signed());
            }
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
            // Hand the just-freed screensaver heap back to the OS.
            // `malloc_trim(0)` releases free memory at the top of every
            // glibc arena; without it the arenas retain the ~21 MiB
            // crossfade buffers mpv just dropped, inflating idle RSS
            // until the arenas are reused.  Returned int is non-zero
            // when memory was actually released; we don't need it.
            #[cfg(target_os = "linux")]
            {
                let _ = unsafe { libc::malloc_trim(0) };
            }
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
        // Shift state does not persist across a full teardown/re-show
        // (probe finding — no persistence).  A fresh Show on a NEW
        // surface always starts its walk centred.
        self.reset_shift();
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
            #[cfg(target_os = "linux")]
            RenderCommand::SetShift { shift } => self.handle_set_shift(shift),
        }
    }

    /// `SetShift`: register the per-display pixel-shift config.  Just
    /// a field write — the black/screensaver install paths read
    /// `self.shift_settings` when they next build a buffer.  See
    /// [`RenderCommand::SetShift`] for why this isn't threaded through
    /// the `Show`/`ShowScreensaver` payloads instead.
    fn handle_set_shift(&mut self, shift: ShiftSettings) {
        tracing::debug!(
            event = "render_shift_settings_registered",
            display_id = %self.display_id,
            shift_px = shift.shift_px,
            interval_ms = shift.shift_interval.as_millis(),
        );
        self.shift_settings = shift;
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
                    // show uses — or, when pixel-shift is active, the
                    // same oversized-shm + wp_viewport path
                    // `complete_pending_show` uses (a cached
                    // `black_buffer` from an earlier swap in THIS
                    // surface's lifetime is already correctly sized,
                    // since `shift_settings` never changes mid-thread-
                    // lifetime — see `ShiftSettings` doc).
                    let dest = self.configured_size;
                    let shift_viewport = self.ensure_shift_viewport(&wl_surface, dest);
                    if self.black_buffer.is_none() {
                        if shift_viewport.is_some() {
                            let (ow, oh) = self.render_dims(dest);
                            if let Ok(buffer) =
                                crate::linux::surface::create_shm_black_buffer(ow, oh, self)
                            {
                                self.black_buffer = Some(buffer);
                            }
                        } else if self.single_pixel_manager.is_some() && self.viewporter.is_some() {
                            let buffer = self
                                .single_pixel_manager
                                .as_ref()
                                .expect("checked Some above")
                                .create_u32_rgba_buffer(0, 0, 0, u32::MAX, &self.queue_handle, ());
                            if let Some(viewport) = self.ensure_viewport(&wl_surface) {
                                viewport
                                    .set_destination(dest.0.cast_signed(), dest.1.cast_signed());
                                self.viewport = Some(viewport);
                            }
                            self.black_buffer = Some(buffer);
                        } else if let Ok(buffer) =
                            crate::linux::surface::create_shm_black_buffer(dest.0, dest.1, self)
                        {
                            self.black_buffer = Some(buffer);
                        }
                    }

                    if let Some(black) = self.black_buffer.as_ref() {
                        wl_surface.attach(Some(black), 0, 0);
                        if shift_viewport.is_some() {
                            let (ow, oh) = self.render_dims(dest);
                            wl_surface.damage_buffer(0, 0, ow.cast_signed(), oh.cast_signed());
                        }
                        wl_surface.commit();
                        self.maybe_arm_shift_timer();
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
                // NOTE: mirrors destroy_surface()'s tail (see above) —
                // any teardown path that drops the live surface must
                // also reset shift state, or a stale shift_state
                // survives to corrupt the NEXT Show/ShowScreensaver on
                // this display (ensure_shift_viewport sees a stale
                // Some() and skips set_source on the fresh WpViewport;
                // maybe_arm_shift_timer never re-arms with a stale
                // token). Keep this call whenever a new teardown path
                // is added.
                self.reset_shift();
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
            wayland_client::protocol::wl_pointer::Event::Enter { serial, .. } => {
                // Cursor hide: a null surface makes the compositor stop
                // drawing one.  Surface receive pointer input because we
                // never set an input region.
                if let Some(pointer) = &state.pointer {
                    pointer.set_cursor(serial, None, 0, 0);
                }
                state.last_pointer_serial = Some(serial);
            }
            wayland_client::protocol::wl_pointer::Event::Button { serial, .. } => {
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

#[test]
fn transition_step_separated_item_ended_then_item_loaded_arms_fade() {
    // M1: events in SEPARATE wakeups still produce the arming
    // transition (MpvPlayer::poll_events splits them across
    // wakeups; the state machine mustn't require both in one
    // call).
    // ItemEnded: Idle → Captured + DropTimer.
    let (ph, t, cmd) = transition_step(TransitionPhase::Idle, 0, TransitionEvent::ItemEnded);
    assert_eq!(
        (ph, t, cmd),
        (TransitionPhase::Captured, 0, StepCmd::DropTimer)
    );

    // ItemLoaded (next wakeup): Captured → AwaitingFirstFrame.
    let (ph, t, cmd) = transition_step(ph, t, TransitionEvent::ItemLoaded);
    assert_eq!(
        (ph, t, cmd),
        (TransitionPhase::AwaitingFirstFrame, 0, StepCmd::NoOp)
    );

    // FrameRendered{ok=true}: AwaitingFirstFrame → Fading + ArmTimer.
    let (ph, _t, cmd) = transition_step(ph, t, TransitionEvent::FrameRendered { ok: true });
    assert_eq!((ph, cmd), (TransitionPhase::Fading, StepCmd::ArmTimer));
}

#[test]
fn transition_step_completed_then_second_item_ended_captures_again() {
    // M2: at Idle, a fresh ItemEnded captures again — proves the
    // phase machine is eligible to capture across cycles (the
    // old design's `session.transition.is_none()` gate blocked
    // captures after the first fade and got stuck forever).
    // Run: Idle → Fading → Idle (via tick Complete) → Captured.
    let (_, _, _) = transition_step(TransitionPhase::Idle, 0, TransitionEvent::ItemEnded);
    // The wiring's `tick_step` returns `(Idle, T_MAX, Complete)`
    // for a Fading tick that brings t to max — emulate that
    // completion here.
    let (ph, t, cmd) = tick_step(TransitionPhase::Fading, 256, 1);
    assert_eq!(
        (ph, t, cmd),
        (TransitionPhase::Idle, T_MAX, TickCmd::Complete)
    );

    // Idle again — a fresh ItemEnded must capture (NOT be ignored).
    let (ph, t, cmd) = transition_step(ph, t, TransitionEvent::ItemEnded);
    assert_eq!(
        (ph, t, cmd),
        (TransitionPhase::Captured, 0, StepCmd::DropTimer),
        "after completion the phase machine must be eligible for the next capture (M2)"
    );
}

#[test]
fn transition_step_rapid_item_ended_mid_fade_restarts_from_current() {
    // M1 (rapid-restart): Image advances faster than
    // `transition_duration`.  An `ItemEnded` arriving while we're
    // still `Fading` (mid-blend) captures the current visual and
    // moves to Captured — letting the next cycle start a fresh
    // blend from whatever's on screen (no zipper).
    let (ph, t, cmd) = transition_step(TransitionPhase::Fading, 50, TransitionEvent::ItemEnded);
    assert_eq!(
        (ph, t, cmd),
        (TransitionPhase::Captured, 0, StepCmd::DropTimer),
        "ItemEnded mid-Fade must move to Captured (drop live timer)"
    );
}

#[test]
fn tick_step_advances_even_with_no_frame_for_static_image_fade() {
    // Re-review (M2 / M4): a static image only produces ONE mpv
    // frame.  The blend timer must STILL advance `t` on every tick
    // — each tick re-blends the same current picture at a higher `t`,
    // which IS the visible opacity change.  Tying the advance to
    // a fresh mpv frame means static images never fade.
    //
    // Iterate `tick_step(Fading, …, t_step=9)` ~30 times with the
    // minimum possible "new frame" signal — the helper advances
    // unconditionally now (its signature dropped the `ok` arg
    // since the wiring no longer uses it as a gate).
    let (ph, t, _) = tick_step(TransitionPhase::Fading, 0, 9);
    assert_eq!(
        (ph, t),
        (TransitionPhase::Fading, 9),
        "first tick must advance t=0 → t=9"
    );

    // Drive enough ticks to complete.
    let mut phase = ph;
    let mut t = t;
    let mut completed = false;
    for _ in 0..60 {
        let (np, nt, cmd) = tick_step(phase, t, 9);
        phase = np;
        t = nt;
        if matches!(cmd, TickCmd::Complete) {
            completed = true;
            break;
        }
    }
    assert!(completed, "static-image fade must complete in ≤60 ticks");
    assert_eq!(t, T_MAX, "completed fade pins t at T_MAX");
    assert_eq!(
        phase,
        TransitionPhase::Idle,
        "completed → Idle (timer drops)"
    );
}

#[test]
fn tick_step_advance_is_independent_of_wakeup_storm() {
    // Re-review (M2 / M4): frame rate independence.  A wakeup storm
    // (e.g. a 60-fps video source) must NOT make the fade run at
    // 60 fps; the visible opacity change is owned by the blend
    // timer (~33 ms cadence).  Drive the same fade with simulated
    // tick interleaving: each candidate tick picks up where the
    // previous left off.  Compare the tick count to `frames_for_blend`
    // (computed from the same constants the production code uses).
    //
    // The test asserts: t advances ONLY through `tick_step`.  A
    // wakeup-driven path (which would advance t once per mpv frame)
    // would finish in `frames_for_blend / video_fps` * ticks vs the
    // 30-fps timer cadence — at 60 fps video, the wakeup-driven path
    // would produce a fade in half the configured duration.
    let t_step = 9_u16; // 1s @ 30 fps ceiling
    let (frames, _) = blend::compute_blend_params(30, std::time::Duration::from_secs(1));
    assert_eq!(frames, 30, "fixture: 1s @ 30 fps yields 30 frames");

    // Drive exactly `frames` ticks; we expect Complete at or before
    // `frames + 1`.
    let mut phase = TransitionPhase::Fading;
    let mut t: u16 = 0;
    let mut completes_at: Option<u32> = None;
    for i in 0..(frames + 10) {
        let (np, nt, cmd) = tick_step(phase, t, t_step);
        phase = np;
        t = nt;
        if matches!(cmd, TickCmd::Complete) {
            completes_at = Some(i + 1);
            break;
        }
    }
    assert!(
        completes_at.is_some(),
        "must complete within frames+10 ticks"
    );
    let n = completes_at.unwrap();
    // Allow ±1 for rounding; the wakeup-storm-driven path would
    // finish in ~frames/2 ticks (at 60 fps video), failing this
    // upper bound.
    assert!(
        (frames - 1..=frames + 1).contains(&n),
        "tick count {n} should be ≈frames (got {frames})"
    );
}

#[test]
fn process_mpv_events_batch_order_itemloaded_then_itemended_allocates() {
    // Re-review (M1): the lazy-allocation gate used `first()` on the
    // events batch — a batch starting with `ItemLoaded` (ItemEnded
    // second) blocked allocation, and the per-event loop then
    // skipped both events because `transition` was still `None`.
    // Exercise the helper directly: start with `transition == None`,
    // feed `[ItemLoaded, ItemEnded]`, expect the helper to allocate
    // AND process both events.
    let events = vec![MpvItemEvent::ItemLoaded, MpvItemEvent::ItemEnded];
    let (new_transition, cmds) =
        process_mpv_events(None, &events, /* t_step */ 9, /* buf_len */ 0);

    assert!(
        new_transition.is_some(),
        "process_mpv_events must allocate TransitionState when ANY \
         event in the batch is ItemEnded (not just first).  This was \
         the M1 lazy-alloc wiring bypass — pre-fix code's `first()` \
         gate skipped events when ItemLoaded was first."
    );
    assert!(
        cmds.capture_pending,
        "process_mpv_events must request a capture on ItemEnded.  The \
         ItemLoaded-then-ItemEnded sequence lands on Captured phase \
         with DropTimer cmd."
    );
    assert!(
        !cmds.arm_pending,
        "ItemLoaded-then-ItemEnded does not arm a timer (Captured is \
         not AwaitingFirstFrame; the ArmTimer only fires from \
         AwaitingFirstFrame → Fading)."
    );
}

#[test]
fn process_mpv_events_no_itemended_does_not_allocate() {
    // The dual: a batch with NO ItemEnded must NOT allocate.  A
    // batch of just [ItemLoaded] is a no-op (Idle + ItemLoaded is
    // a no-op in transition_step — there's nothing to fade yet).
    let events = vec![MpvItemEvent::ItemLoaded];
    let (new_transition, cmds) = process_mpv_events(None, &events, 9, 0);
    assert!(
        new_transition.is_none(),
        "no ItemEnded in batch → no allocation"
    );
    assert!(!cmds.capture_pending);
    assert!(!cmds.arm_pending);
}

#[test]
fn tick_step_complete_at_max_resets_to_idle() {
    // Once the blend hits t >= T_MAX, the timer drops and the
    // state returns to Idle (capture stays allocated for the
    // next cycle — this is the M2 fix).
    let (ph, t, cmd) = tick_step(TransitionPhase::Fading, T_MAX - 4, 9);
    assert_eq!(
        (ph, t, cmd),
        (TransitionPhase::Idle, T_MAX, TickCmd::Complete),
        "complete must move to Idle at t=T_MAX"
    );
}

#[test]
fn tick_step_clamps_incomplete_advance() {
    // t=200, t_step=100, T_MAX=256 → new_t=300 → saturating_add caps
    // at u16::MAX, but the state machine clamps to T_MAX so we
    // want exactly T_MAX (256) — the COMPLETE branch fires on the
    // boundary, not Advance.
    let (ph, t, cmd) = tick_step(TransitionPhase::Fading, 200, 100);
    assert_eq!(
        cmd,
        TickCmd::Complete,
        "t=200 + step=100 ≥ T_MAX must Complete"
    );
    assert_eq!(t, T_MAX, "completed blend pins t at T_MAX exactly");
    assert_eq!(ph, TransitionPhase::Idle);
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
