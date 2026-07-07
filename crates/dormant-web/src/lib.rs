//! `dormant-web` — local, loopback-only web dashboard for dormant.
//!
//! # Architecture
//!
//! This crate is an axum HTTP/WS bridge that reads live daemon state from
//! cloned engine channels and serves a dashboard SPA.  It depends on
//! [`dormant_core`] (types, channels, config) and [`dormant_doctor`]
//! (live-owned-state diagnostics) but NOT on `dormantd` — the daemon
//! creates a [`WebState`] from its own handles and calls [`spawn`].
//!
//! # Feature gate
//!
//! The crate is an optional dependency of `dormantd`, gated behind the
//! `web-ui` Cargo feature.  When the feature is off, zero web code is
//! compiled and the daemon binary is byte-identical to M1.

mod assets;
mod config_patch;
mod error;
mod routes;
mod security;
mod server;
mod state;

use std::net::SocketAddr;

use tokio::task::JoinHandle;

pub use state::{WebState, WebStateInner};

/// Spawn the web server on `bind`, returning a [`JoinHandle`] for the
/// server task together with the resolved [`SocketAddr`] (useful when
/// `bind` uses port 0 for an ephemeral assignment).
///
/// The resolved address is reported back via a oneshot from inside the
/// spawned task, so the caller receives it after a successful bind (no
/// port-release race).
///
/// # Errors
///
/// Returns [`std::io::Error`] if the spawned task drops before binding
/// or the addr-report oneshot is cancelled.
pub async fn spawn(
    bind: SocketAddr,
    state: WebState,
) -> std::io::Result<(JoinHandle<()>, SocketAddr)> {
    // ── Startup hygiene: prune stale temp files ─────────────────────────────
    // config.toml.tmp.* files older than 1 hour are leftovers from a
    // previous apply that crashed before it could clean up.
    prune_stale_temps(&state.inner.config_path);

    let (addr_tx, addr_rx) = tokio::sync::oneshot::channel();

    let handle = tokio::spawn(async move {
        if let Err(e) = server::serve_and_report(bind, state, addr_tx).await {
            tracing::error!(event = "web_server_exited", error = %e);
        }
    });

    let addr = addr_rx
        .await
        .map_err(|_| std::io::Error::other("web server task dropped before binding"))?;

    tracing::info!(
        event = "web_listening",
        bind = %addr,
        "dormant web UI started"
    );

    Ok((handle, addr))
}

/// Remove `config.toml.tmp.*` files older than 1 hour from the config
/// directory.  These are leftovers from a previous apply that crashed
/// before it could clean up its temp file; they are safe to delete
/// because any apply writing them is long dead.
pub(crate) fn prune_stale_temps(config_path: &std::path::Path) {
    let dir = match config_path.parent() {
        Some(d) => d,
        None => return,
    };

    let cutoff =
        match std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(3600)) {
            Some(t) => t,
            None => return,
        };

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("config.toml.tmp.") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::{ControlMsg, StateSnapshot};
    use dormant_doctor::DoctorService;
    use indexmap::IndexMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio_util::sync::CancellationToken;

    /// Build a fake engine that responds to `ControlMsg::Snapshot` with a
    /// canned [`StateSnapshot`].  Spawned as a background task; returns the
    /// sender so the test can inject messages.
    fn spawn_fake_engine(snapshot: StateSnapshot) -> mpsc::Sender<ControlMsg> {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::Snapshot(tx) = msg {
                    let _ = tx.send(snapshot.clone());
                }
            }
        });
        ctl_tx
    }

    /// Build a minimal [`WebState`] with fake channels for the test.
    /// Only `ctl_tx` and `cancel` carry real data; the other receivers
    /// hold dummy senders that are kept alive for the test's duration.
    fn test_web_state(ctl_tx: mpsc::Sender<ControlMsg>) -> (WebState, CancellationToken) {
        let cancel = CancellationToken::new();

        let (reload_trigger_tx, reload_trigger_rx) = mpsc::channel::<()>(8);
        let (reload_tx, reload_rx) = broadcast::channel(16);

        let config = Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        });
        let creds = Arc::new(Credentials::default());

        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);

        // Keep senders alive so the receivers don't close.
        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);
        std::mem::forget(reload_trigger_rx);

        let doctor = DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let state = WebState::new(WebStateInner {
            ctl_tx,
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: PathBuf::from("/dev/null"),
            creds_path: PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                8080,
            ),
            cancel: cancel.clone(),
        });

        (state, cancel)
    }

    /// The critical smoke test: spawn on loopback with an ephemeral port,
    /// GET `/api/state`, receive 200 + the canned snapshot JSON, then
    /// cancel gracefully.
    #[tokio::test]
    async fn spawn_binds_loopback_and_serves_state() {
        let snapshot = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
        };

        let ctl_tx = spawn_fake_engine(snapshot.clone());
        let (state, cancel) = test_web_state(ctl_tx);

        let bind: SocketAddr = ([127, 0, 0, 1], 0).into();
        let (handle, addr) = spawn(bind, state)
            .await
            .expect("spawn should bind loopback:0");

        // Build the URL and make the GET request.
        let url = format!("http://{addr}/api/state");
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("GET /api/state should succeed");
        assert_eq!(resp.status(), 200, "expected 200 OK, got {}", resp.status());

        let body: serde_json::Value = resp.json().await.unwrap();
        let expected: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(body, expected);

        // Shut down.
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("server should shut down within 5s")
            .expect("server task should not panic");
    }

    /// After cancellation, the server shuts down and the join handle
    /// resolves.
    #[tokio::test]
    async fn spawn_shuts_down_gracefully_on_cancel() {
        let snapshot = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
        };

        let ctl_tx = spawn_fake_engine(snapshot);
        let (state, cancel) = test_web_state(ctl_tx);

        let bind: SocketAddr = ([127, 0, 0, 1], 0).into();
        let (handle, _addr) = spawn(bind, state)
            .await
            .expect("spawn should bind loopback:0");

        cancel.cancel();
        // The join handle should resolve promptly (within 5s).
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("server should shut down within 5s")
            .expect("server task should not panic");
    }
}
