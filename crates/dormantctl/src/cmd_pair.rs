//! `dormantctl pair` — Samsung Tizen TV pairing.
//!
//! Pairs with a Samsung Tizen TV that needs an auth token before
//! the daemon can control it.

use std::path::PathBuf;

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

    println!("{}", connecting_message(&args.host, PAIR_TIMEOUT));

    let token = rt
        .block_on(dormant_displays::samsung_tizen::pair(
            &args.host,
            PAIR_TIMEOUT,
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
/// How long pairing waits for the TV to connect and grant a token.
const PAIR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The line printed before pairing starts.
///
/// The connect step only completes once the TV has granted a token, so there
/// is no point at which the TV is known to be showing its prompt; the message
/// is conditional and names the timeout instead.
fn connecting_message(host: &str, timeout: std::time::Duration) -> String {
    format!(
        "Connecting to {host}… If the TV shows an \"Allow dormant\" prompt, accept it \
         (giving up after {}s).",
        timeout.as_secs()
    )
}

fn store_token(creds_path: &std::path::Path, host: &str, token: &str) -> anyhow::Result<()> {
    config::upsert_samsung_token(creds_path, host, token)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connecting_message_is_conditional_and_names_the_timeout() {
        let msg = connecting_message("192.0.2.10", PAIR_TIMEOUT);
        assert!(msg.contains("192.0.2.10"), "{msg}");
        assert!(
            msg.contains("If the TV shows an \"Allow dormant\" prompt"),
            "the prompt must be described as conditional: {msg}"
        );
        assert!(msg.contains("giving up after 60s"), "{msg}");
        assert!(
            !msg.contains("— accept"),
            "must not claim a prompt is already showing: {msg}"
        );
    }

    #[test]
    fn store_token_writes_samsung_entry() {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");
        let host = "192.0.2.7";
        let token = "abc123-token";

        store_token(&creds_path, host, token).unwrap();

        let raw = std::fs::read_to_string(&creds_path).unwrap();
        assert!(
            raw.contains("[samsung]") || raw.contains("samsung"),
            "credentials file should contain samsung table: {raw}"
        );
        assert!(
            raw.contains("192.0.2.7"),
            "credentials file should contain host key: {raw}"
        );
        assert!(
            raw.contains("abc123-token"),
            "credentials file should contain token: {raw}"
        );
    }
}
