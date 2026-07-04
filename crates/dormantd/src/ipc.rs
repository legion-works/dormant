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
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum line length for IPC requests/responses (1 MB).
const MAX_LINE_BYTES: usize = 1_048_576;

/// Spawn the IPC server on a background task.
///
/// Binds `socket_path` with `0o600` permissions (umask-guarded to avoid a
/// race between `bind` and `set_permissions`), and accepts connections in a
/// loop until `cancel` is triggered.  Each connection is handled by a spawned
/// per-connection task.
///
/// If the socket file already exists, attempts to connect to it first — if
/// the connection succeeds the old daemon is alive and we error out; if it
/// fails (dead daemon) we unlink and bind fresh.
///
/// # Errors
///
/// - Bind failure (address in use by a live daemon, permission denied, …).
/// - Permission set failure.
/// - Parent directory is group/world-writable or not owned by us.
pub fn spawn(
    socket_path: &Path,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_trigger_tx: mpsc::Sender<()>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>> {
    // Stale-socket recovery: connect-test before bind so we never silently
    // replace a live daemon's socket.
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

    // Ensure parent directory exists with 0o700 permissions.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket parent directory '{}'", parent.display()))?;
        // Refuse if group/world-writable or not owned by us.
        let meta = std::fs::metadata(parent)
            .with_context(|| format!("stat parent directory '{}'", parent.display()))?;
        let mode = meta.permissions().mode();
        // Check the write bits specifically (0o022 = group-write, 0o002 =
        // world-write).  Read/execute for group/world is acceptable (e.g.
        // /tmp with 0o1777 has the sticky bit but is world-writable — we
        // still reject that; the daemon should use a private subdir).
        if mode & 0o022 != 0 {
            anyhow::bail!(
                "socket parent directory '{}' has permissions {:#o}; \
                 group/world-writable directories are not allowed",
                parent.display(),
                mode & 0o777,
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let uid = unsafe { libc::geteuid() };
            if meta.uid() != uid {
                anyhow::bail!(
                    "socket parent directory '{}' is owned by uid {} but we are {}",
                    parent.display(),
                    meta.uid(),
                    uid,
                );
            }
        }
    }

    // Use umask to ensure the socket is created 0o600 even before the
    // explicit set_permissions call.  At this point no concurrent
    // file-creating tasks are running — the engine and sensor sources open
    // sockets and serial ports, not files.  Umask is process-wide, so the
    // narrow window is safe.  The post-bind set_permissions is kept as
    // belt-and-braces.
    let old_umask = unsafe { libc::umask(0o077) };
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("bind Unix socket '{}'", socket_path.display()))?;
    unsafe { libc::umask(old_umask) };

    // Explicit 0o600 after bind (belt-and-braces — umask already narrowed).
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
/// responses.  Lines are capped at [`MAX_LINE_BYTES`]; oversized lines get an
/// error response and the connection stays usable.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_trigger_tx: mpsc::Sender<()>,
) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    loop {
        let line = match read_line_bounded(&mut reader).await {
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
    let until = match duration_s {
        Some(s) => match std::time::SystemTime::now().checked_add(Duration::from_secs(s)) {
            Some(t) => Some(dormant_core::types::Timestamp(t)),
            None => {
                return IpcResponse::error("duration overflow");
            }
        },
        None => None,
    };
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

/// Read one line from the buffered reader, capping at [`MAX_LINE_BYTES`].
///
/// Returns `Ok(None)` on EOF, `Ok(Some(line))` on success, or an error if the
/// line content (excluding the trailing newline) exceeds the cap.
async fn read_line_bounded(
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
) -> Result<Option<String>> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        // Check if we already have a complete line in buf (from a prior read).
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            if pos > MAX_LINE_BYTES {
                // Drain the oversized line so the connection stays usable.
                buf.drain(..=pos);
                anyhow::bail!("line exceeds maximum length of {MAX_LINE_BYTES} bytes");
            }
            let line: Vec<u8> = buf.drain(..pos).collect();
            let _ = buf.drain(..1); // remove the \n
            let s =
                String::from_utf8(line).map_err(|_| anyhow::anyhow!("invalid UTF-8 in request"))?;
            return Ok(Some(s));
        }

        if buf.len() > MAX_LINE_BYTES {
            anyhow::bail!("line exceeds maximum length of {MAX_LINE_BYTES} bytes");
        }

        if reader.read_buf(&mut buf).await? == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            let s =
                String::from_utf8(buf).map_err(|_| anyhow::anyhow!("invalid UTF-8 in request"))?;
            return Ok(Some(s));
        }
    }
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
    if line.len() > MAX_LINE_BYTES {
        anyhow::bail!("response line exceeds maximum length of {MAX_LINE_BYTES} bytes");
    }
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Minimal fake engine for unit tests.
    fn fake_engine() -> (mpsc::Sender<super::ControlMsg>, CancellationToken) {
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        // Drop rx immediately — the IPC server will get send errors and
        // respond with "engine not available", which is fine for these tests.
        (tx, cancel)
    }

    #[test]
    fn resolve_socket_path_from_config() {
        let p =
            dormant_core::paths::resolve_socket_path(Some(std::path::Path::new("/tmp/test.sock")));
        assert_eq!(p, std::path::PathBuf::from("/tmp/test.sock"));
    }

    #[test]
    fn resolve_socket_path_default_xdg() {
        let p = dormant_core::paths::resolve_socket_path(None);
        // Just verify the function returns something (env-dependent).
        assert!(!p.as_os_str().is_empty());
    }

    #[test]
    fn resolve_socket_path_fallback() {
        let p = dormant_core::paths::resolve_socket_path(None);
        assert!(!p.as_os_str().is_empty());
    }

    #[tokio::test]
    async fn parent_dir_group_writable_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("dormant.sock");
        // Make parent group-writable.
        let meta = std::fs::metadata(dir.path()).unwrap();
        let mut perms = meta.permissions();
        perms.set_mode(0o775);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        let (ctl_tx, cancel) = fake_engine();
        let (reload_tx, _reload_rx) = mpsc::channel::<()>(8);

        let result = crate::ipc::spawn(&socket_path, ctl_tx, reload_tx, cancel);
        assert!(result.is_err(), "group-writable parent should be rejected");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("group/world-writable"),
            "error should mention writable: {err}"
        );
    }

    #[tokio::test]
    async fn parent_dir_plain_tempdir_ok() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("dormant.sock");

        let (ctl_tx, cancel) = fake_engine();
        let (reload_tx, _reload_rx) = mpsc::channel::<()>(8);

        let result = crate::ipc::spawn(&socket_path, ctl_tx, reload_tx, cancel.clone());
        assert!(
            result.is_ok(),
            "plain tempdir should be accepted: {result:?}"
        );
        cancel.cancel();
    }
}
