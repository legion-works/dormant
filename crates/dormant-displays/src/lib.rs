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
pub mod ddcci;
pub mod executor;
pub mod ha_passthrough;
pub mod kwin_dpms;
pub mod registry;
pub mod samsung_ip;
pub mod samsung_tizen;
pub mod vcp_ops;
