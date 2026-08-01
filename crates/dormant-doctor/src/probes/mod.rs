//! Probe modules — one file per probe category.
//!
//! Each module mirrors the convention: config, mqtt, ha, usb, ddcci, samsung.

pub mod config;
pub mod ddcci;
pub mod ha;
pub mod input_filter;
#[cfg(target_os = "macos")]
pub mod macos_display_catalog;
pub mod macos_display_sleep;
pub mod macos_idle;
pub mod macos_power;
pub mod macos_power_off;
pub mod mqtt;
pub mod samsung;
pub mod usb;
pub mod wear_sampling;
