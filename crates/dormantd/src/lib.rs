//! dormant daemon library.
//!
//! Wires configuration → sensors → zones → rules → displays, with post-probe
//! display validation, hot config reload, and a user-activity inhibitor. The
//! `dormantd` binary is a thin wrapper over [`app::App`].

pub mod app;
pub mod boot_guard;
pub mod idle_source;
pub mod inhibit_activity;
#[cfg(unix)]
pub mod ipc;
pub mod logging;
pub mod notifier;
pub mod reload;
pub mod sd_notify;
pub mod single_instance;
pub mod wear_tracker;
