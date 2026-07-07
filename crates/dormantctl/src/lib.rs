//! `dormantctl` library surface.
//!
//! The binary owns the CLI surface (`main.rs`); the [`client`] module is
//! re-exported here so out-of-process consumers — most notably
//! `dormant-tray` — can drive the same IPC protocol without re-implementing
//! the socket glue (the M2 doctor-extraction lesson: sharing a client
//! across crates beats drifting copies).

pub mod client;
