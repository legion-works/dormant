//! dormant daemon library.
//!
//! Wires configuration → sensors → zones → rules → displays, with post-probe
//! display validation, hot config reload, and a user-activity inhibitor. The
//! `dormantd` binary is a thin wrapper over [`app::App`].

pub mod app;
pub mod inhibit_activity;
pub mod logging;
pub mod reload;
