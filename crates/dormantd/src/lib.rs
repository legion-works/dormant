//! dormant daemon library.
//!
//! Wires configuration → sensors → zones → rules → displays, with post-probe
//! display validation, hot config reload, and a user-activity inhibitor. The
//! `dormantd` binary is a thin wrapper over [`app::App`].

pub mod activity_follow;
pub mod app;
mod audio_policy;
pub mod audio_source;
pub mod boot;
pub mod boot_guard;

mod coordination_poll;
pub mod direct_switch;
#[cfg(target_os = "linux")]
pub mod evdev_idle;
pub mod filtered_activity;
pub mod gamma_recovery;
pub mod hooks;
pub mod idle_observation;
pub mod idle_source;
pub mod inhibit_activity;
pub mod inhibit_audio;
#[cfg(unix)]
pub mod ipc;
pub mod logging;
#[cfg(target_os = "macos")]
pub mod macos_input_filter;

pub mod macos_idle;
pub mod notifier;
pub mod reload;
pub mod sd_notify;
pub mod single_instance;
mod watchdog_schedule;
pub mod wear_tracker;
