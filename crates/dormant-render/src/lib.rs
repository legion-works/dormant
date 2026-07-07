//! The local Wayland render backend — layer-shell overlays implementing
//! [`dormant_core::traits::RenderSink`].
//!
//! This crate manages the lifecycle of render surfaces on an output:
//!
//! - **black overlay**: full-screen black layer, the primary software-
//!   blank fallback when all hardware controllers fail or are unreachable.
//! - **screensaver overlay**: last-resort idle surface, shown only when
//!   the full ladder (controllers → black → screensaver) has been exhausted
//!   and the display is still unblanked.  Not implemented in this backend
//!   yet — see `LayerShellRenderSink::show`.
//!
//! All Wayland I/O is [`target_os = "linux"`]-gated.  On non-Linux the crate
//! is a no-op binary balloon — the engine's fall-through logic already
//! handles the case where no `RenderSink` is registered.
//!
//! ## Module layout
//!
//! | Module | Task | Purpose |
//! |--------|------|---------|
//! | `latch` | T7 | First-input-event latch (testable pure data) |
//! | `command` | T6 | Cross-thread command / reply encoding |
//! | `stub` | T6 | Non-Linux stub (cross-compile green) |
//! | `linux` | T6+7 | Real Wayland layer-shell backend (`linux` only) |
//!
//! ## Thread model
//!
//! Wayland objects are not `Send` — every [`wayland_client::Proxy`] is
//! bound to the thread that created it.  The [`LayerShellRenderSink`] is
//! therefore a lightweight handle: it owns a [`calloop::channel::Sender`]
//! for the show/teardown command channel.  A dedicated OS thread
//! (spawned by [`LayerShellRenderSink::new`]) owns the actual
//! [`wayland_client::Connection`], the layer surface, and the
//! input-wake channel; it runs a [`calloop::EventLoop`] that drains the
//! command channel.  Wayland I/O is driven inline from inside the
//! channel callback (`event_queue.roundtrip`) to avoid racing the
//! calloop FD read against an in-band roundtrip.
//!
//! [`calloop::channel::Sender`]: calloop::channel::Sender

#![warn(missing_docs)]

mod command;
mod latch;

pub mod playlist;
pub mod settings;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod screensaver;

#[cfg(not(target_os = "linux"))]
mod stub;

// Re-export the screensaver settings type so the daemon can build one
// from a `dormant_core::config::ScreensaverConfig` and pass it into
// the sink.  Available on all platforms — the stub sink just ignores
// the settings.
pub use settings::{ScaleMode, ScreensaverSettings, TransitionMode};

// Linux uses the real Wayland backend; non-Linux uses the stub.  Both
// expose a `LayerShellRenderSink` with the same surface so consumers
// can `use dormant_render::LayerShellRenderSink` unconditionally.
#[cfg(target_os = "linux")]
pub use linux::LayerShellRenderSink;

#[cfg(not(target_os = "linux"))]
pub use stub::LayerShellRenderSink;
