//! Unix-domain socket IPC server for `dormantctl` communication.
//!
//! Listens on a Unix socket (path from config, with an XDG-based default
//! chain), accepts line-delimited JSON [`IpcRequest`]s, dispatches them to
//! the engine via [`ControlMsg`] channels, and writes [`IpcResponse`]s (or
//! `DaemonEvent` streams) back.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
use dormant_core::rules::{ControlMsg, StateSnapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Resolve the socket path from an optional config value.
///
/// Default chain:
/// 1. `$XDG_RUNTIME_DIR/dormant.sock`
/// 2. `/run/dormant/dormant.sock`
#[must_use]
pub fn resolve_socket_path(config_socket: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Some(p) = config_socket {
        return p.to_path_buf();
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let mut p = std::path::PathBuf::from(runtime_dir);
        p.push("dormant.sock");
        return p;
    }
    std::path::PathBuf::from("/run/dormant/dormant.sock")
}

/// Spawn the IPC server on a background task.
///
/// Binds `socket_path`, sets permissions to `0o600`, and accepts connections
/// in a loop until `cancel` is triggered.  Each connection is handled by a
/// spawned per-connection task.
///
/// If the socket file already exists, attempts to connect to it first — if
/// the connection succeeds the old daemon is alive and we error out; if it
/// fails (dead daemon) we unlink and bind fresh.
///
/// # Errors
///
/// - Bind failure (address in use by a live daemon, permission denied, …).
/// - Permission set failure.
pub fn spawn(
    socket_path: &Path,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_trigger_tx: mpsc::Sender<()>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>> {
    // Stale-socket recovery: if the file exists, try connecting.  If the
    // connection succeeds the old daemon is alive — bail.  If it fails
    // (ENOENT / ECONNREFUSED / …) the socket is stale; unlink and proceed.
    if socket_path.exists() {
        match std::os::unix::net::UnixStream::connect(socket_path) {
            Ok(_) => {
                anyhow::bail!(
                    "socket '{}' is already in use by a running daemon",
                    socket_path.display()
                );
            }
            Err(_) => {
                let _ = std::fs::remove_file(socket_path);
            }
        }
    }

    // Ensure parent directory exists.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket parent directory '{}'", parent.display()))?;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("bind Unix socket '{}'", socket_path.display()))?;

    // Set permissions to 0o600.
    let metadata = std::fs::metadata(socket_path)
        .with_context(|| format!("stat socket '{}'", socket_path.display()))?;
    let mut perms = metadata.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(socket_path, perms)
        .with_context(|| format!("set socket permissions '{}'", socket_path.display()))?;

    tracing::info!(event = "ipc_listening", socket = %socket_path.display());

    let socket_owned = socket_path.to_path_buf();
    let handle = tokio::spawn(async move {
        run(listener, ctl_tx, reload_trigger_tx, cancel, &socket_owned).await;
    });

    Ok(handle)
}

/// The accept loop — runs until cancelled.
async fn run(
    listener: UnixListener,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_trigger_tx: mpsc::Sender<()>,
    cancel: CancellationToken,
    socket_path: &std::path::Path,
) {
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!(event = "ipc_shutdown", socket = %socket_path.display());
                let _ = std::fs::remove_file(socket_path);
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, addr)) => {
                        let ctl = ctl_tx.clone();
                        let reload = reload_trigger_tx.clone();
                        tokio::spawn(handle_connection(stream, ctl, reload));
                        let _ = addr; // Unix socket peer address (debug).
                    }
                    Err(e) => {
                        tracing::error!(event = "ipc_accept_error", error = %e);
                    }
                }
            }
        }
    }
}

/// Handle one client connection: read line-delimited JSON, dispatch, write
/// responses.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_trigger_tx: mpsc::Sender<()>,
) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => return, // clean disconnect
            Err(e) => {
                tracing::warn!(event = "ipc_read_error", error = %e);
                return;
            }
        };

        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        let request: IpcRequest = match serde_json::from_str(&trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = IpcResponse::error(format!("bad request: {e}"));
                let _ = write_json(&mut writer, &resp).await;
                continue;
            }
        };

        match request {
            IpcRequest::Status => {
                let resp = handle_status(&ctl_tx).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::Pause { rule, duration_s } => {
                let resp = handle_pause(&ctl_tx, rule, duration_s).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::Resume { rule } => {
                let resp = handle_resume(&ctl_tx, rule).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::Blank { display } => {
                let resp = handle_blank(&ctl_tx, &display).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::Wake { display } => {
                let resp = handle_wake(&ctl_tx, &display).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::Events => {
                handle_events(&ctl_tx, &mut writer).await;
                return; // events stream owns the connection until disconnect
            }
            IpcRequest::Reload => {
                let _ = reload_trigger_tx.send(()).await;
                let resp = IpcResponse::ok(None);
                let _ = write_json(&mut writer, &resp).await;
            }
        }
    }
}

// ── Request handlers ──────────────────────────────────────────────────────────

/// Fetch a snapshot and return it.
async fn handle_status(ctl_tx: &mpsc::Sender<ControlMsg>) -> IpcResponse {
    match request_snapshot(ctl_tx).await {
        Some(snap) => IpcResponse::ok(Some(snap)),
        None => IpcResponse::error("engine not available"),
    }
}

/// Pause blanking.
async fn handle_pause(
    ctl_tx: &mpsc::Sender<ControlMsg>,
    rule: Option<String>,
    duration_s: Option<u64>,
) -> IpcResponse {
    let rule_id = rule.map(dormant_core::types::RuleId);
    let until = duration_s.map(|s| {
        dormant_core::types::Timestamp(std::time::SystemTime::now() + Duration::from_secs(s))
    });
    let msg = ControlMsg::Pause {
        rule: rule_id,
        until,
    };
    if ctl_tx.send(msg).await.is_err() {
        return IpcResponse::error("engine not available");
    }
    IpcResponse::ok(None)
}

/// Resume blanking.
async fn handle_resume(ctl_tx: &mpsc::Sender<ControlMsg>, rule: Option<String>) -> IpcResponse {
    let rule_id = rule.map(dormant_core::types::RuleId);
    let msg = ControlMsg::Resume { rule: rule_id };
    if ctl_tx.send(msg).await.is_err() {
        return IpcResponse::error("engine not available");
    }
    IpcResponse::ok(None)
}

/// Force-blank a display — validates the display name against a snapshot first.
async fn handle_blank(ctl_tx: &mpsc::Sender<ControlMsg>, display: &str) -> IpcResponse {
    if let Some(err) = validate_display_name(ctl_tx, display).await {
        return err;
    }
    let msg = ControlMsg::ForceBlank(dormant_core::types::DisplayId(display.to_string()));
    if ctl_tx.send(msg).await.is_err() {
        return IpcResponse::error("engine not available");
    }
    IpcResponse::ok(None)
}

/// Force-wake a display — validates the display name against a snapshot first.
async fn handle_wake(ctl_tx: &mpsc::Sender<ControlMsg>, display: &str) -> IpcResponse {
    if let Some(err) = validate_display_name(ctl_tx, display).await {
        return err;
    }
    let msg = ControlMsg::ForceWake(dormant_core::types::DisplayId(display.to_string()));
    if ctl_tx.send(msg).await.is_err() {
        return IpcResponse::error("engine not available");
    }
    IpcResponse::ok(None)
}

/// Subscribe to the event stream and write events as JSON lines until the
/// client disconnects or the stream lags.
async fn handle_events(
    ctl_tx: &mpsc::Sender<ControlMsg>,
    writer: &mut tokio::io::WriteHalf<tokio::net::UnixStream>,
) {
    let (tx, rx) = oneshot::channel();
    let msg = ControlMsg::SubscribeEvents(tx);
    if ctl_tx.send(msg).await.is_err() {
        let _ = write_json(writer, &IpcResponse::error("engine not available")).await;
        return;
    }
    let Ok(mut events) = rx.await else { return };

    loop {
        match events.recv().await {
            Ok(event) => {
                let line = serde_json::to_string(&event).unwrap_or_default();
                if write_line(writer, &line).await.is_err() {
                    return; // client disconnected
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let lagged = serde_json::json!({"event":"stream_lagged","skipped":n});
                let line = serde_json::to_string(&lagged).unwrap_or_default();
                if write_line(writer, &line).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                return;
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Fetch a snapshot from the engine (bounded).
async fn request_snapshot(ctl_tx: &mpsc::Sender<ControlMsg>) -> Option<StateSnapshot> {
    let (tx, rx) = oneshot::channel();
    if ctl_tx.send(ControlMsg::Snapshot(tx)).await.is_err() {
        return None;
    }
    tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .ok()?
        .ok()
}

/// Validate that a display name exists in the current engine snapshot.
/// Returns `Some(error_response)` if the display is unknown.
async fn validate_display_name(
    ctl_tx: &mpsc::Sender<ControlMsg>,
    display: &str,
) -> Option<IpcResponse> {
    let snap = request_snapshot(ctl_tx).await?;
    let known: std::collections::HashSet<&str> =
        snap.displays.iter().map(|(id, _)| id.as_str()).collect();
    if !known.contains(display) {
        return Some(IpcResponse::error(format!("unknown display '{display}'")));
    }
    None
}

/// Serialize a value as a JSON line and write it to the stream.
async fn write_json<T: serde::Serialize>(
    writer: &mut tokio::io::WriteHalf<tokio::net::UnixStream>,
    value: &T,
) -> Result<()> {
    let line = serde_json::to_string(value)?;
    write_line(writer, &line).await
}

/// Write a line (plus newline) to the stream.
async fn write_line(
    writer: &mut tokio::io::WriteHalf<tokio::net::UnixStream>,
    line: &str,
) -> Result<()> {
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_socket_path_from_config() {
        let p = resolve_socket_path(Some(Path::new("/tmp/test.sock")));
        assert_eq!(p, std::path::PathBuf::from("/tmp/test.sock"));
    }

    #[test]
    fn resolve_socket_path_default_xdg() {
        // Temporarily set XDG_RUNTIME_DIR
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: test-only env manipulation, single-threaded test.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        let p = resolve_socket_path(None);
        assert_eq!(p, std::path::PathBuf::from("/run/user/1000/dormant.sock"));
        match prev {
            Some(v) => unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", v);
            },
            None => unsafe {
                std::env::remove_var("XDG_RUNTIME_DIR");
            },
        }
    }

    #[test]
    fn resolve_socket_path_fallback() {
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: test-only env manipulation, single-threaded test.
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let p = resolve_socket_path(None);
        assert_eq!(p, std::path::PathBuf::from("/run/dormant/dormant.sock"));
        match prev {
            Some(v) => unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", v);
            },
            None => unsafe {
                std::env::remove_var("XDG_RUNTIME_DIR");
            },
        }
    }
}
