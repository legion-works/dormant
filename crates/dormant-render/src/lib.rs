//! The local Wayland render backend — layer-shell overlays implementing
//! [`dormant_core::traits::RenderSink`].
//!
//! This crate manages the lifecycle of render surfaces on an output:
//!
//! - **black overlay** (Task 6): full-screen black layer, the primary software-
//!   blank fallback when all hardware controllers fail or are unreachable.
//! - **screensaver overlay** (Phase 2 / Task 7): last-resort idle surface,
//!   shown only when the full ladder (controllers → black → screensaver) has
//!   been exhausted and the display is still unblanked.
//!
//! All Wayland I/O is [`target_os = "linux"`]-gated.  On non-Linux the crate
//! is a no-op binary balloon — the engine's fall-through logic already
//! handles the case where no `RenderSink` is registered.
//!
//! ## Planned modules (landing in Task 6–7)
//!
//! | Module | Task | Purpose |
//! |--------|------|---------|
//! | `wayland` | T6 | Connection management, `wl_display` lifecycle |
//! | `layer_shell` | T6 | `zwlr_layer_surface_v1` helpers |
//! | `black` | T6 | `RenderBlack` surface |
//! | `input_wake` | T7 | Pointer/key input forwarding for instant wake |
