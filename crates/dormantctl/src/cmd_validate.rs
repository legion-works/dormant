//! `dormantctl validate` — offline config validation.
//!
//! Loads and validates the configuration without connecting to a running
//! daemon.  Reuses the same default-path logic as `dormantd`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use dormant_core::config::{Strictness, load_config, load_credentials, validate};
use dormant_core::paths;
use dormant_displays::registry::capabilities;

/// Validate the configuration offline.
#[derive(Parser, Debug)]
pub struct ValidateArgs {
    /// Path to the config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Path to the credentials file.
    #[arg(long)]
    pub credentials: Option<PathBuf>,

    /// Treat unknown config keys as warnings instead of errors.
    #[arg(long)]
    pub lenient_keys: bool,
}

/// Run the `validate` command.
///
/// # Errors
///
/// Propagates I/O errors.
pub fn run(args: &ValidateArgs) -> Result<()> {
    let strictness = if args.lenient_keys {
        Strictness::Warn
    } else {
        Strictness::Strict
    };

    let config_path =
        paths::resolve_config_path(args.config.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let creds_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| paths::sibling_credentials(&config_path));

    let (cfg, warnings) = load_config(&config_path, strictness)?;
    for w in &warnings {
        println!("warning [{}]: {}", w.key_path, w.message);
    }

    let creds = load_credentials(&creds_path)?;

    let errors = validate(&cfg, &capabilities(), &creds);
    if errors.is_empty() {
        println!("configuration OK");
        Ok(())
    } else {
        for e in &errors {
            println!("{e}");
        }
        std::process::exit(1);
    }
}
