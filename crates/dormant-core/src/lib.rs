//! Core domain types, traits, zone and rules engines for dormant — pure logic, no I/O.
#![warn(missing_docs)]

pub mod config;
pub mod error;
pub mod ipc_proto;
pub mod paths;
pub mod reload;
pub mod rules;
pub mod state_machine;
pub mod traits;
pub mod types;
pub mod zone;

#[cfg(feature = "test-fakes")]
pub mod fakes;
