//! `dormantctl pair` — device pairing commands.
//!
//! Pairs with network-connected devices that need an auth token before
//! the daemon can control them (e.g. Samsung Tizen TVs).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dormant_core::config;
use dormant_core::ipc_proto::{CoordinationDiscoveredPeer, CoordinationPeers, IpcRequest};
use dormant_core::paths;

const INSTANCE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const INSTANCE_DISCOVERY_RETRY: Duration = Duration::from_millis(500);

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

    let selected = resolve_instance_with_retry(
        name,
        instance_id,
        || {
            let inventory =
                dormantctl::client::send_request(socket, &IpcRequest::CoordinationPeersList)?;
            inventory.coordination_peers.ok_or_else(|| {
                anyhow::anyhow!(
                    inventory
                        .error
                        .unwrap_or_else(|| "pairing unavailable".into())
                )
            })
        },
        std::thread::sleep,
        || println!("discovering instances…"),
    )?;
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

fn resolve_instance_with_retry(
    name: &str,
    instance_id: Option<&str>,
    mut fetch_inventory: impl FnMut() -> anyhow::Result<CoordinationPeers>,
    mut wait: impl FnMut(Duration),
    mut on_first_retry: impl FnMut(),
) -> anyhow::Result<CoordinationDiscoveredPeer> {
    let deadline = Instant::now() + INSTANCE_DISCOVERY_TIMEOUT;
    let mut retried = false;
    loop {
        let peers = fetch_inventory()?;
        if let Some(peer) = select_discovered_instance(peers, name, instance_id)? {
            return Ok(peer);
        }
        if Instant::now() >= deadline {
            return no_discovered_instance(name, instance_id);
        }
        if !retried {
            on_first_retry();
            retried = true;
        }
        wait(INSTANCE_DISCOVERY_RETRY);
    }
}

fn select_discovered_instance(
    peers: CoordinationPeers,
    name: &str,
    instance_id: Option<&str>,
) -> anyhow::Result<Option<CoordinationDiscoveredPeer>> {
    if let Some(instance_id) = instance_id {
        return Ok(peers
            .discovered
            .into_iter()
            .find(|peer| peer.instance_id == instance_id));
    }
    let matches: Vec<_> = peers
        .discovered
        .into_iter()
        .filter(|peer| peer.display_name == name)
        .collect();
    match matches.as_slice() {
        [peer] => Ok(Some(peer.clone())),
        [] => Ok(None),
        many => {
            let ids = many
                .iter()
                .map(|peer| peer.instance_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("multiple instances named '{name}': {ids}; retry with --instance-id")
        }
    }
}

fn no_discovered_instance(
    name: &str,
    instance_id: Option<&str>,
) -> anyhow::Result<CoordinationDiscoveredPeer> {
    if let Some(instance_id) = instance_id {
        anyhow::bail!("no discovered instance '{instance_id}' named '{name}'");
    }
    anyhow::bail!("no discovered instance named '{name}'")
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
    use dormant_core::ipc_proto::{CoordinationDiscoveredPeer, CoordinationPeers};

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

    #[test]
    fn instance_resolution_retries_until_mdns_discovery_populates_the_inventory() {
        let peer = CoordinationDiscoveredPeer {
            instance_id: "peer-id".to_owned(),
            display_name: "Living room".to_owned(),
            pairing_port: 9_999,
            window_id: "window-id".to_owned(),
        };
        let mut inventories = vec![
            CoordinationPeers {
                discovered: Vec::new(),
                paired: Vec::new(),
            },
            CoordinationPeers {
                discovered: vec![peer.clone()],
                paired: Vec::new(),
            },
        ]
        .into_iter();
        let mut retry_count = 0;

        let selected = resolve_instance_with_retry(
            "Living room",
            Some("peer-id"),
            || Ok(inventories.next().unwrap()),
            |_| {},
            || retry_count += 1,
        )
        .unwrap();

        assert_eq!(selected, peer);
        assert_eq!(retry_count, 1);
    }
}
