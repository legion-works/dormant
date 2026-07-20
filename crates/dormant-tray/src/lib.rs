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
//! Linux/macOS:
//!
//! - [`ipc_loop`] — the reconnecting event-stream reader driving the tray's
//!   shared state.
//!
//! Linux-only:
//! - [`tray`] — the [`ksni::Tray`] implementation.
//!
//! macOS-only:
//! - [`tray_macos`] — the `AppKit` status-item implementation.
//!
//! ## Crate target
//!
//! `ksni` is Linux-only; the Tokio-backed IPC loop is shared by Linux and
//! macOS. Platform-specific dependencies remain cfg-gated so portability
//! checks stay green.

#![warn(missing_docs)]

/// Tagged projection of menu actions for platform target/action callbacks.
pub mod action_table;
/// Pure action planning and injected platform I/O execution.
#[cfg(unix)]
pub mod dispatch;
pub mod icon;
pub mod menu;
pub mod state;
/// Pure monochrome pixel renderer for macOS template tray icons.
pub mod template_icon;
pub mod tooltip;
/// Cross-platform state shared by tray frontends and the IPC loop.
pub mod tray_state;

/// Default port for the M2 web UI.  The daemon does not expose its bound
/// `web_port` through [`dormant_core::rules::StateSnapshot`]; we fall back
/// to this constant when opening "Open web UI" until the daemon adds a
/// `web_url` field to the snapshot.
pub const DEFAULT_WEB_PORT: u16 = 8137;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod ipc_loop;
#[cfg(target_os = "linux")]
pub mod tray;
#[cfg(target_os = "macos")]
pub mod tray_macos;
