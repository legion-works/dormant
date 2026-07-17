//! Display controllers for dormant: one module per controller type, plus a
//! static registry and the per-display [`executor::DisplayExecutor`] that turns
//! the rules-engine's `CommandSink` calls into an ordered fallback chain
//! with bounded wake-retry bursts.
//!
//! ## Why a separate crate?
//!
//! Display controllers perform real I/O (process spawn, network, `DBus`) and
//! must not leak into [`dormant_core`].  The core crate owns the trait
//! surface and types; this crate owns the implementations.

#![warn(missing_docs)]

pub mod command;
pub mod ddc_lock;
pub mod ddcci;
pub mod executor;
pub mod gamma_breadcrumb;
pub mod ha_passthrough;
pub mod kwin_dpms;
// `macos_display_catalog` is the thin macOS-only FFI backend (raw Quartz
// gamma-table calls) — cfg-gated at the `mod` declaration (belt-and-braces
// with the file's own `#![cfg(target_os = "macos")]`) so it never attempts
// to compile on a target with no CoreGraphics framework. `macos_gamma_black`
// is platform-neutral (its controller logic and `FakeGammaApi` tests run on
// any host); only its `#[cfg(target_os = "macos")]`-gated `new` constructor
// reaches into `macos_display_catalog`.
#[cfg(target_os = "macos")]
pub mod macos_display_catalog;
pub mod macos_display_sleep;
pub mod macos_gamma_black;
// `macos_power` is the thin macOS-only FFI backend for `macos_display_sleep`
// (raw IOPM assertion + CoreGraphics per-display asleep readback calls) —
// cfg-gated at the `mod` declaration for the same reason as
// `macos_display_catalog` above.
#[cfg(target_os = "macos")]
pub mod macos_power;
pub mod registry;
pub mod samsung_ip;
pub mod samsung_tizen;
#[cfg(feature = "test-util")]
pub mod test_support;
pub mod vcp_ops;
