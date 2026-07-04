//! `dormantctl validate` — offline config validation.
//!
//! Loads and validates the configuration without connecting to a running
//! daemon.  Reuses the same default-path logic as `dormantd`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use dormant_core::config::{Strictness, load_config, load_credentials, validate};
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

    let config_path = resolve_config_path(args.config.as_deref())?;
    let creds_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| sibling_credentials(&config_path));

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

/// Resolve the config path from an explicit arg or default chain.
fn resolve_config_path(explicit: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    // Same default chain as dormantd main.rs
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg).join("dormant").join("config.toml");
        if p.exists() {
            return Ok(p);
        }
    } else if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home)
            .join(".config")
            .join("dormant")
            .join("config.toml");
        if p.exists() {
            return Ok(p);
        }
    }
    let etc = PathBuf::from("/etc/dormant/config.toml");
    if etc.exists() {
        return Ok(etc);
    }
    anyhow::bail!(
        "no config file found; pass --config or create \
         $XDG_CONFIG_HOME/dormant/config.toml or /etc/dormant/config.toml"
    );
}

/// `credentials.toml` in the same directory as the config file.
fn sibling_credentials(config_path: &std::path::Path) -> PathBuf {
    config_path.parent().map_or_else(
        || PathBuf::from("credentials.toml"),
        |dir| dir.join("credentials.toml"),
    )
}
