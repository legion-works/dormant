//! `dormant-tray` — KDE `StatusNotifierItem` applet for `dormantd`.
//!
//! The crate exposes a library surface so the binary's modules can be
//! reasoned about independently of `ksni`'s single-instance daemon wiring,
//! and so future tests can drive the menu/state logic without a D-Bus
//! session bus.
//!
//! ## Module layout
//!
//! - [`state`] — pure-logic icon-state derivation from a `StateSnapshot`.
//! - [`tooltip`] — pure-logic tooltip construction.
//! - [`menu`] — pure-logic menu model construction (without `ksni` types so
//!   it can be unit-tested against canned snapshots).
//! - [`dispatch`] — pure action plans plus injected platform I/O execution.
//! - [`icon`] — pixmap construction + runtime variant overlays (paused
//!   badge, greying).  Pure pixel ops, no D-Bus.
//! - [`tray_state`] — cross-platform state shared by tray frontends and the
//!   IPC loop.
//!
//! Linux-only:
//!
//! - [`ipc_loop`] — the reconnecting event-stream reader driving the tray's
//!   shared state.
//! - [`tray`] — the [`ksni::Tray`] implementation.
//!
//! ## Crate target
//!
//! Linux only.  The `ksni` and `tokio` deps are gated on
//! `target_os = "linux"` so `cargo check --workspace` stays green on
//! Windows/macOS portability legs (memory-1718 class — the recurring
//! cross-platform CI gauntlet).

#![warn(missing_docs)]

/// Pure action planning and injected platform I/O execution.
#[cfg(unix)]
pub mod dispatch;
pub mod icon;
pub mod menu;
pub mod state;
pub mod tooltip;
/// Cross-platform state shared by tray frontends and the IPC loop.
pub mod tray_state;

/// Default port for the M2 web UI.  The daemon does not expose its bound
/// `web_port` through [`dormant_core::rules::StateSnapshot`]; we fall back
/// to this constant when opening "Open web UI" until the daemon adds a
/// `web_url` field to the snapshot.
pub const DEFAULT_WEB_PORT: u16 = 8137;

#[cfg(target_os = "linux")]
pub mod ipc_loop;
#[cfg(target_os = "linux")]
pub mod tray;
