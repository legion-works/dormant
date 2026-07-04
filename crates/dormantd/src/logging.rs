//! Tracing subscriber setup for the daemon.
//!
//! The filter is taken from `RUST_LOG` when set, otherwise from the config's
//! `daemon.log_level`. `--log-json` selects the structured JSON formatter;
//! otherwise a human-readable formatter is used. Both include the target
//! module and the literal `event = "..."` fields the code emits (grep-stable).

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

/// Initialise the global tracing subscriber.
///
/// `level` is the fallback directive used when `RUST_LOG` is unset (typically
/// `daemon.log_level`). `json` selects the JSON formatter.
///
/// # Errors
///
/// Returns an error if a global subscriber was already installed.
pub fn init(level: &str, json: bool) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);

    if json {
        builder
            .json()
            .try_init()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("install JSON tracing subscriber")
    } else {
        builder
            .try_init()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("install tracing subscriber")
    }
}
