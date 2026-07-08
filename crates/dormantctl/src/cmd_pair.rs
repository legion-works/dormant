//! `dormantctl pair` — device pairing commands.
//!
//! Pairs with network-connected devices that need an auth token before
//! the daemon can control them (e.g. Samsung Tizen TVs).

use std::path::PathBuf;
use std::time::Duration;

use dormant_core::config;
use dormant_core::paths;

/// Arguments for the `pair` command.
pub struct PairArgs {
    /// Path to the config file.
    pub config: Option<PathBuf>,
    /// Path to the credentials file.
    pub credentials: Option<PathBuf>,
    /// TV hostname or IP address.
    pub host: String,
}

/// Run the `pair samsung` subcommand.
///
/// Connects to the TV, prompts the user to accept the pairing request on the
/// TV, and stores the returned token in the credentials file.
///
/// # Errors
///
/// Returns an error if pairing fails (timeout, connection refused, etc.) or if
/// the token cannot be written to the credentials file.
pub fn run(args: &PairArgs) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;

    println!(
        "Connecting to {} — accept the 'Allow dormant' prompt on your TV…",
        args.host
    );

    let token = rt
        .block_on(dormant_displays::samsung_tizen::pair(
            &args.host,
            Duration::from_secs(60),
        ))
        .map_err(|e| anyhow::anyhow!("pairing failed: {e}"))?;

    let config_path =
        paths::resolve_config_path(args.config.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let creds_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| paths::sibling_credentials(&config_path));

    store_token(&creds_path, &args.host, &token)?;

    println!(
        "Paired. Token stored for {} in {}.",
        args.host,
        creds_path.display()
    );
    Ok(())
}

/// Write a Samsung pairing token into the credentials file.
///
/// Delegates to [`dormant_core::config::upsert_samsung_token`].
fn store_token(creds_path: &std::path::Path, host: &str, token: &str) -> anyhow::Result<()> {
    config::upsert_samsung_token(creds_path, host, token)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_token_writes_samsung_entry() {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");
        let host = "10.1.1.7";
        let token = "abc123-token";

        store_token(&creds_path, host, token).unwrap();

        let raw = std::fs::read_to_string(&creds_path).unwrap();
        assert!(
            raw.contains("[samsung]") || raw.contains("samsung"),
            "credentials file should contain samsung table: {raw}"
        );
        assert!(
            raw.contains("10.1.1.7"),
            "credentials file should contain host key: {raw}"
        );
        assert!(
            raw.contains("abc123-token"),
            "credentials file should contain token: {raw}"
        );
    }
}
