//! HTTP route handlers for the dormant web API.
//!
//! Each module maps one URL prefix; the [`crate::server`] module mounts them
//! into the axum router.

pub(crate) mod command;
pub(crate) mod config;
pub(crate) mod config_apply;
pub(crate) mod doctor;
pub(crate) mod events;
pub(crate) mod wear;
