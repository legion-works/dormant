//! dormant daemon library.
//!
//! Wires configuration → sensors → zones → rules → displays, with post-probe
//! display validation, hot config reload, and a user-activity inhibitor. The
//! `dormantd` binary is a thin wrapper over [`app::App`].

pub mod app;
mod audio_policy;
pub mod audio_source;
pub mod boot;
pub mod boot_guard;
mod coordination_poll;
pub mod gamma_recovery;
pub mod idle_source;
pub mod inhibit_activity;
pub mod inhibit_audio;
#[cfg(unix)]
pub mod ipc;
pub mod logging;
pub mod macos_idle;
pub mod notifier;
pub mod reload;
pub mod sd_notify;
pub mod single_instance;
mod watchdog_schedule;
pub mod wear_tracker;

/// Compile-time dependency probe for the instance-pairing implementation.
///
/// Task 11 replaces this once the pairing runtime owns these crates directly.
#[doc(hidden)]
#[allow(unused_imports)]
mod pairing_dep_probe {
    use base64 as _;
    use ed25519_dalek as _;
    use hmac as _;
    use mdns_sd as _;
    use rand_core as _;
    use sha2 as _;
    use spake2 as _;
}
