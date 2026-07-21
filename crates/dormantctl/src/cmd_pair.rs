//! `dormantctl pair` — device pairing commands.
//!
//! Pairs with network-connected devices that need an auth token before
//! the daemon can control them (e.g. Samsung Tizen TVs).

use std::path::{Path, PathBuf};
use std::time::Duration;

use dormant_core::config;
use dormant_core::ipc_proto::IpcRequest;
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

/// Run the `pair instance` subcommand through the daemon IPC boundary.
pub fn run_instance(
    socket: &Path,
    name: &str,
    code: Option<&str>,
    instance_id: Option<&str>,
    open: bool,
) -> anyhow::Result<()> {
    if open {
        let response = dormantctl::client::send_request(
            socket,
            &IpcRequest::CoordinationPairOpen {
                display_name: name.to_owned(),
            },
        )?;
        let opened = response.coordination_pair_open.ok_or_else(|| {
            anyhow::anyhow!(
                response
                    .error
                    .unwrap_or_else(|| "pairing unavailable".into())
            )
        })?;
        println!("Pairing window open for {name}.");
        println!("Code: {}", opened.code);
        println!("Expires: {}", opened.expires_at);
        return Ok(());
    }

    let inventory = dormantctl::client::send_request(socket, &IpcRequest::CoordinationPeersList)?;
    let peers = inventory.coordination_peers.ok_or_else(|| {
        anyhow::anyhow!(
            inventory
                .error
                .unwrap_or_else(|| "pairing unavailable".into())
        )
    })?;
    let matches: Vec<_> = peers
        .discovered
        .into_iter()
        .filter(|peer| peer.display_name == name)
        .collect();
    let selected = match (instance_id, matches.as_slice()) {
        (Some(id), _) => matches
            .into_iter()
            .find(|peer| peer.instance_id == id)
            .ok_or_else(|| anyhow::anyhow!("no discovered instance '{id}' named '{name}'"))?,
        (None, [peer]) => peer.clone(),
        (None, []) => anyhow::bail!("no discovered instance named '{name}'"),
        (None, many) => {
            let ids = many
                .iter()
                .map(|peer| peer.instance_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("multiple instances named '{name}': {ids}; retry with --instance-id")
        }
    };
    let response = dormantctl::client::send_request(
        socket,
        &IpcRequest::CoordinationPairJoin {
            display_name: selected.display_name,
            instance_id: selected.instance_id,
            code: code.expect("clap requires --code unless --open").to_owned(),
        },
    )?;
    if !response.ok {
        anyhow::bail!(response.error.unwrap_or_else(|| "pairing failed".into()));
    }
    println!("Pairing request sent for {name}.");
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
