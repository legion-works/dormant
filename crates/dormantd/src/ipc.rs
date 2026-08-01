//! Unix-domain socket IPC server for `dormantctl` communication.
//!
//! Listens on a Unix socket (path from config, with an XDG-based default
//! chain), accepts line-delimited JSON [`IpcRequest`]s, dispatches them to
//! the engine via [`ControlMsg`] channels, and writes [`IpcResponse`]s (or
//! `DaemonEvent` streams) back.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dormant_core::ipc_proto::{BlankRequestMode, IpcRequest, IpcResponse, WearSamplingStatus};
use dormant_core::observation::ReloadSource;
use dormant_core::reload::ReloadRequester;
use dormant_core::rules::{ControlMsg, DaemonEvent, StateSnapshot};
use dormant_doctor::DoctorService;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::active_sampler::{ActiveSamplerHandle, SamplerCommand, SamplerError, SamplingState};
use crate::direct_switch::{DirectSwitchHandle, SwitchReason};

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
/// `doctor_service` is intercepted by the connection handler for
/// [`IpcRequest::Doctor`] (a non-engine path — see the module docstring).
///
/// # Errors
///
/// - Bind failure (address in use by a live daemon, permission denied, …).
/// - Permission set failure.
/// - Parent directory is group/world-writable or not owned by us.
pub fn spawn(
    socket_path: &Path,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_requester: ReloadRequester,
    doctor_service: DoctorService,
    direct_switch: Arc<DirectSwitchHandle>,
    active_sampler: Option<ActiveSamplerHandle>,
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
        run(
            listener,
            ctl_tx,
            reload_requester,
            doctor_service,
            direct_switch,
            active_sampler,
            cancel,
            &socket_owned,
        )
        .await;
    });

    Ok(handle)
}

/// The accept loop — runs until cancelled.
#[allow(
    clippy::too_many_arguments,
    reason = "IPC dependencies remain explicit at the daemon lifecycle boundary."
)]
async fn run(
    listener: UnixListener,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_requester: ReloadRequester,
    doctor_service: DoctorService,
    direct_switch: Arc<DirectSwitchHandle>,
    active_sampler: Option<ActiveSamplerHandle>,
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
                        let reload = reload_requester.clone();
                        let doctor = doctor_service.clone();
                        let ds = direct_switch.clone();
                        let sampler = active_sampler.clone();
                        tokio::spawn(handle_connection(stream, ctl, reload, doctor, ds, sampler));
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
#[allow(
    clippy::too_many_lines,
    reason = "IPC variants deliberately remain visible in one exhaustive dispatch match."
)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    ctl_tx: mpsc::Sender<ControlMsg>,
    reload_requester: ReloadRequester,
    doctor_service: DoctorService,
    direct_switch: Arc<DirectSwitchHandle>,
    active_sampler: Option<ActiveSamplerHandle>,
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
            IpcRequest::Blank { display, mode } => {
                let resp = handle_blank(&ctl_tx, &display, mode).await;
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
                let _ = reload_requester.notify(ReloadSource::Ipc).await;
                let resp = IpcResponse::ok(None);
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::Doctor => {
                // Intercepted before the engine path: doctor reports
                // owned-state from the live snapshot + active network
                // probes, never re-opens held handles, and is
                // coalesced across concurrent callers.
                let report = doctor_service.run().await;
                let resp = IpcResponse::doctor(report);
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::EmergencyWake => {
                let resp = handle_emergency_wake(&ctl_tx).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::Exercise { display } => {
                let resp = handle_exercise(&ctl_tx, &display).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::SwitchToLocal { display } => {
                let resp = handle_switch_local(&direct_switch, &display).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::SwitchToPeer { display } => {
                let resp = handle_switch_peer(&direct_switch, &display).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::WearSamplingEnable => {
                let resp = handle_wear_enable(active_sampler.as_ref()).await;
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::WearSamplingStatus => {
                let resp = handle_wear_status(active_sampler.as_ref());
                let _ = write_json(&mut writer, &resp).await;
            }
            IpcRequest::WearSamplingDisable { forget } => {
                let resp = handle_wear_disable(active_sampler.as_ref(), forget).await;
                let _ = write_json(&mut writer, &resp).await;
            }
        }
    }
}

async fn handle_wear_enable(active_sampler: Option<&ActiveSamplerHandle>) -> IpcResponse {
    let Some(active_sampler) = active_sampler else {
        return IpcResponse::wear_sampling(WearSamplingStatus::Error(
            "wear_sampling_unsupported".to_owned(),
        ));
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(error) = active_sampler
        .send(SamplerCommand::Enable { reply: reply_tx })
        .await
    {
        return IpcResponse::wear_sampling(WearSamplingStatus::Error(sampler_error_reason(&error)));
    }
    let status = match reply_rx.await {
        Ok(status) => status.into_ipc_status(),
        Err(_) => WearSamplingStatus::Error("wear_sampling_command_closed".to_owned()),
    };
    IpcResponse::wear_sampling(status)
}

fn handle_wear_status(active_sampler: Option<&ActiveSamplerHandle>) -> IpcResponse {
    let Some(active_sampler) = active_sampler else {
        return IpcResponse::wear_sampling(WearSamplingStatus::Error(
            "wear_sampling_unsupported".to_owned(),
        ));
    };
    let status = active_sampler.status().borrow().state;
    let status = match status {
        SamplingState::ConsentPending => WearSamplingStatus::AwaitingConsent,
        SamplingState::NeedsConsent => {
            WearSamplingStatus::Error("wear_sampling_needs_consent".to_owned())
        }
        SamplingState::Connecting | SamplingState::Streaming => WearSamplingStatus::Granted,
        SamplingState::Disabled | SamplingState::Suspended | SamplingState::Cooldown => {
            WearSamplingStatus::Error("wear_sampling_not_active".to_owned())
        }
    };
    IpcResponse::wear_sampling(status)
}

async fn handle_wear_disable(
    active_sampler: Option<&ActiveSamplerHandle>,
    forget: bool,
) -> IpcResponse {
    let Some(active_sampler) = active_sampler else {
        return IpcResponse::wear_sampling(WearSamplingStatus::Error(
            "wear_sampling_unsupported".to_owned(),
        ));
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(error) = active_sampler
        .send(SamplerCommand::Disable {
            forget,
            reply: reply_tx,
        })
        .await
    {
        return IpcResponse::wear_sampling(WearSamplingStatus::Error(sampler_error_reason(&error)));
    }
    match reply_rx.await {
        Ok(Ok(())) => IpcResponse::wear_sampling(WearSamplingStatus::Granted),
        Ok(Err(error)) => {
            IpcResponse::wear_sampling(WearSamplingStatus::Error(sampler_error_reason(&error)))
        }
        Err(_) => IpcResponse::wear_sampling(WearSamplingStatus::Error(
            "wear_sampling_command_closed".to_owned(),
        )),
    }
}

fn sampler_error_reason(error: &SamplerError) -> String {
    match error {
        SamplerError::DisabledByConfig => "wear_sampling_disabled".to_owned(),
        SamplerError::FlowAlreadyActive => "wear_sampling_flow_already_active".to_owned(),
        SamplerError::NoGraphicalSession => "wear_sampling_no_graphical_session".to_owned(),
        SamplerError::CommandChannelClosed => "wear_sampling_command_closed".to_owned(),
        SamplerError::Store(_) => "wear_sampling_store_error".to_owned(),
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

/// Blank a display — validates the display name against a snapshot first, then
/// dispatches either the soft ladder path or the operator-override hardware
/// path per the requested [`BlankRequestMode`].  Issue #124 forced this split:
/// `Hard` issues a `PowerOff` that hard-powers a shared panel and can take the
/// USB hub (and downstream sensors) down with it, while `Soft` walks the
/// configured render/stage/controller ladder from its first stage and is the
/// safe default.
async fn handle_blank(
    ctl_tx: &mpsc::Sender<ControlMsg>,
    display: &str,
    mode: BlankRequestMode,
) -> IpcResponse {
    if let Some(err) = validate_display_name(ctl_tx, display).await {
        return err;
    }
    let target = dormant_core::types::DisplayId(display.to_string());
    let msg = match mode {
        BlankRequestMode::Soft => ControlMsg::SoftBlank(target),
        BlankRequestMode::Hard => ControlMsg::ForceBlank(target),
    };
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

/// Bounded-wait wrapper around `ControlMsg::EmergencyWake`.
///
/// The CLI's `emergency-wake` path uses this as the IPC fast-path.  The
/// 2-second window is the budget for "the daemon is healthy and we should
/// route through it" — beyond it the CLI falls back to constructing display
/// controllers directly from the loaded config + credentials (so a wedged
/// daemon never blocks recovery).
async fn handle_emergency_wake(ctl_tx: &mpsc::Sender<ControlMsg>) -> IpcResponse {
    const EMERGENCY_WAKE_IPC_TIMEOUT: Duration = Duration::from_secs(2);

    let (tx, rx) = oneshot::channel();
    let msg = ControlMsg::EmergencyWake { reply: tx };
    if ctl_tx.send(msg).await.is_err() {
        return IpcResponse::error("engine not available");
    }
    match tokio::time::timeout(EMERGENCY_WAKE_IPC_TIMEOUT, rx).await {
        Ok(Ok(report)) => IpcResponse::emergency(report),
        Ok(Err(_recv_dropped)) => IpcResponse::error("emergency_wake: engine dropped reply"),
        Err(_elapsed) => IpcResponse::error("emergency_wake: timed out"),
    }
}

/// Bound on how long the IPC server waits for an `Exercise` reply from the
/// engine.  The engine's actual work (blank → read → wake → read → restore)
/// is bounded internally by per-read timeouts on `read_state`, so this
/// window only bounds the IPC wait — the rule-pause release is guaranteed
/// engine-side regardless (see `ExerciseResume` in the internal results
/// channel), so a timeout here CANNOT strand a paused rule.
const EXERCISE_IPC_TIMEOUT: Duration = Duration::from_secs(20);

/// Dispatch `ControlMsg::Exercise` and wait for the engine's
/// [`ExerciseReport`] reply.  Waits up to [`EXERCISE_IPC_TIMEOUT`].
/// The engine owns the rule-pause window — the IPC layer does not
/// forward a `Resume` (that would strand the pause if this caller
/// timed out or disconnected); the resume fires from the engine's
/// internal results channel as soon as the spawned sequence completes.
async fn handle_exercise(ctl_tx: &mpsc::Sender<ControlMsg>, display: &str) -> IpcResponse {
    if let Some(err) = validate_display_name(ctl_tx, display).await {
        return err;
    }

    let (tx, rx) = oneshot::channel();
    let msg = ControlMsg::Exercise {
        display: dormant_core::types::DisplayId(display.to_string()),
        reply: tx,
    };
    if ctl_tx.send(msg).await.is_err() {
        return IpcResponse::error("engine not available");
    }
    match tokio::time::timeout(EXERCISE_IPC_TIMEOUT, rx).await {
        Ok(Ok(report)) => IpcResponse::exercise(report),
        Ok(Err(_recv_dropped)) => IpcResponse::error("exercise: engine dropped reply"),
        Err(_elapsed) => IpcResponse::error("exercise: timed out"),
    }
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
    let line = serde_json::to_string(&DaemonEvent::Subscribed)
        .expect("DaemonEvent::Subscribed serializes");
    if write_line(writer, &line).await.is_err() {
        return;
    }

    loop {
        match events.recv().await {
            Ok(event) => {
                let line = serde_json::to_string(&event).expect("DaemonEvent serializes");
                if write_line(writer, &line).await.is_err() {
                    return; // client disconnected
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let lagged = serde_json::json!({"event":"stream_lagged","skipped":n});
                let line = serde_json::to_string(&lagged).expect("lagged frame serializes");
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

/// Handle a direct local switch — write the local input code.
async fn handle_switch_local(direct_switch: &DirectSwitchHandle, display: &str) -> IpcResponse {
    let outcome = direct_switch
        .pull(
            dormant_core::types::DisplayId(display.to_string()),
            SwitchReason::Cli,
        )
        .await;
    switch_outcome_to_response(outcome, display)
}

/// Handle a direct peer switch — write the peer input code.
async fn handle_switch_peer(direct_switch: &DirectSwitchHandle, display: &str) -> IpcResponse {
    let outcome = direct_switch
        .push(
            dormant_core::types::DisplayId(display.to_string()),
            SwitchReason::Cli,
        )
        .await;
    switch_outcome_to_response(outcome, display)
}

/// Map a [`SwitchOutcome`] to an [`IpcResponse`], with specific errors for
/// each failure mode so the operator can distinguish "no such display" from
/// "this display isn't shared" from "the write failed."
fn switch_outcome_to_response(
    outcome: crate::direct_switch::SwitchOutcome,
    display: &str,
) -> IpcResponse {
    match outcome {
        crate::direct_switch::SwitchOutcome::Switched => IpcResponse::ok(None),
        crate::direct_switch::SwitchOutcome::NotConfigured => IpcResponse::error(format!(
            "display '{display}' is shared but peer input write code is not configured"
        )),
        crate::direct_switch::SwitchOutcome::Unsupported => IpcResponse::error(format!(
            "display '{display}' is not shared or has no input code configured"
        )),
        crate::direct_switch::SwitchOutcome::HookAborted { reason } => {
            IpcResponse::error(format!("switch hook aborted: {reason}"))
        }
        crate::direct_switch::SwitchOutcome::WriteFailed { error } => {
            IpcResponse::error(format!("write failed: {error}"))
        }
        crate::direct_switch::SwitchOutcome::Cooldown => {
            IpcResponse::error("switch suppressed by activity cooldown")
        }
    }
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
    use indexmap::IndexMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_doctor::DoctorService;

    use super::DirectSwitchHandle;
    use super::switch_outcome_to_response;
    use crate::direct_switch::SwitchOutcome;

    /// Minimal fake engine for unit tests.
    fn fake_engine() -> (mpsc::Sender<super::ControlMsg>, CancellationToken) {
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        // Drop rx immediately — the IPC server will get send errors and
        // respond with "engine not available", which is fine for these tests.
        (tx, cancel)
    }

    /// Build a throwaway [`DirectSwitchHandle`] with no displays — all
    /// switch attempts will return `Unsupported`.
    fn fake_direct_switch(ctl_tx: mpsc::Sender<super::ControlMsg>) -> Arc<DirectSwitchHandle> {
        use std::collections::HashMap;

        let (_, executors_rx) = watch::channel(Arc::new(HashMap::new()));
        let (_, config_rx) = watch::channel(Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }));
        let publisher = Arc::new(crate::hooks::MqttPublisher::new(String::new(), None));
        let hook_engine = Arc::new(crate::hooks::HookEngine::new(publisher));
        Arc::new(DirectSwitchHandle::new(
            executors_rx,
            config_rx,
            hook_engine,
            ctl_tx,
            None,
        ))
    }

    /// Build a throwaway `DoctorService` wired to a dummy channel/watch
    /// (these tests don't exercise the doctor path).
    fn fake_doctor(ctl_tx: mpsc::Sender<super::ControlMsg>) -> DoctorService {
        let (config_tx, config_rx) = watch::channel(Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }));
        let (creds_tx, creds_rx) = watch::channel(Arc::new(Credentials::default()));
        drop(config_tx);
        drop(creds_tx);
        DoctorService::new(ctl_tx, config_rx, creds_rx)
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
        let (reload_tx, _reload_rx) = mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
        let doctor = fake_doctor(ctl_tx.clone());
        let ds = fake_direct_switch(ctl_tx.clone());

        let result = crate::ipc::spawn(
            &socket_path,
            ctl_tx,
            dormant_core::reload::ReloadRequester::new(reload_tx),
            doctor,
            ds,
            None,
            cancel,
        );
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
        let (reload_tx, _reload_rx) = mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
        let doctor = fake_doctor(ctl_tx.clone());
        let ds = fake_direct_switch(ctl_tx.clone());

        let result = crate::ipc::spawn(
            &socket_path,
            ctl_tx,
            dormant_core::reload::ReloadRequester::new(reload_tx),
            doctor,
            ds,
            None,
            cancel.clone(),
        );
        assert!(
            result.is_ok(),
            "plain tempdir should be accepted: {result:?}"
        );
        cancel.cancel();
    }

    // ── Fix C (#138): exit code stability ─────────────────────────────────

    /// `SwitchOutcome::WriteFailed` → `IpcResponse::error(...)` — the error
    /// mapping must always produce `ok: false` so the `dormantctl` CLI exits
    /// non-zero and prints `error:` on stdout.  A missing or silent-success
    /// mapping here is the root cause of the asymmetric exit codes.
    #[test]
    fn write_failed_maps_to_error_response() {
        let outcome = SwitchOutcome::WriteFailed {
            error: "E_DISPLAY_IO: something broke".to_string(),
        };
        let resp = switch_outcome_to_response(outcome, "monitor");
        assert!(!resp.ok, "WriteFailed must produce an error response");
        assert!(
            resp.error
                .as_deref()
                .is_some_and(|e| e.contains("write failed") && e.contains("E_DISPLAY_IO")),
            "error message must preserve the failure detail, got: {:?}",
            resp.error
        );
    }

    /// Every [`SwitchOutcome`] variant must map to a consistent IPC response —
    /// `Switched` → success, everything else → error.  This test is the
    /// exhaustive exit-code contract: a new variant that silently maps to
    /// `ok: true` would mask a failure as success.
    #[test]
    fn every_switch_outcome_has_consistent_response() {
        let outcomes = [
            (SwitchOutcome::Switched, true),
            (
                SwitchOutcome::WriteFailed {
                    error: "E_DISPLAY_IO: fail".to_string(),
                },
                false,
            ),
            (SwitchOutcome::Unsupported, false),
            (SwitchOutcome::NotConfigured, false),
            (
                SwitchOutcome::HookAborted {
                    reason: "timed out".to_string(),
                },
                false,
            ),
            (SwitchOutcome::Cooldown, false),
        ];
        for (outcome, expected_ok) in outcomes {
            let resp = switch_outcome_to_response(outcome, "monitor");
            assert_eq!(
                resp.ok, expected_ok,
                "SwitchOutcome variant must consistently map ok={expected_ok}"
            );
        }
    }

    // ── Blank soft/hard routing (issue #124) ─────────────────────────────
    mod blank_routing {
        use crate::ipc::handle_blank;
        use dormant_core::ipc_proto::BlankRequestMode;
        use dormant_core::rules::{ControlMsg, DisplaySnapshot, StateSnapshot};
        use std::sync::Arc;
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        /// Spawn a fake engine that responds to `ControlMsg::Snapshot` with a
        /// snapshot containing one configured display, and records the LAST
        /// non-snapshot `ControlMsg` it received.
        fn spawn_blank_fake_engine(
            display: &str,
        ) -> (
            mpsc::Sender<ControlMsg>,
            Arc<std::sync::Mutex<Option<ControlMsg>>>,
            CancellationToken,
        ) {
            let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
            let last_msg = Arc::new(std::sync::Mutex::new(None::<ControlMsg>));
            let record = last_msg.clone();
            let snap = StateSnapshot {
                sensors: vec![],
                zones: vec![],
                displays: vec![(
                    display.to_string(),
                    DisplaySnapshot {
                        phase: "active".into(),
                        inhibited: false,
                        paused: false,
                        cmd_gen: 1,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                    },
                )],
                pending_reload: None,
                rollback: None,
                kvm: None,
            };
            tokio::spawn(async move {
                while let Some(msg) = ctl_rx.recv().await {
                    match msg {
                        ControlMsg::Snapshot(tx) => {
                            let _ = tx.send(snap.clone());
                        }
                        other => {
                            *record.lock().unwrap() = Some(other);
                        }
                    }
                }
            });
            (ctl_tx, last_msg, CancellationToken::new())
        }

        /// `Blank { mode: Soft }` must reach the engine as `ControlMsg::SoftBlank`
        /// — the safe ladder path.  This is the wire-boundary contract for
        /// `dormantctl blank` and any legacy `Blank` frame (no `mode`).
        #[tokio::test]
        async fn handle_blank_soft_sends_soft_blank() {
            let (ctl_tx, last_msg, _cancel) = spawn_blank_fake_engine("main");
            let resp = handle_blank(&ctl_tx, "main", BlankRequestMode::Soft).await;
            assert!(resp.ok, "soft blank should succeed: {resp:?}");

            // The recorder task is a separate spawn; give it a tick to land.
            tokio::task::yield_now().await;

            let msg = last_msg.lock().unwrap().take();
            assert!(
                matches!(msg, Some(ControlMsg::SoftBlank(ref id)) if id.0 == "main"),
                "expected ControlMsg::SoftBlank(main), got {msg:?}"
            );
        }

        /// `Blank { mode: Hard }` must reach the engine as `ControlMsg::ForceBlank`
        /// — the operator-override hardware-primary path.  This is the wire-boundary
        /// contract for `dormantctl blank --hard` and the tray/web "Force blank"
        /// buttons.
        #[tokio::test]
        async fn handle_blank_hard_sends_force_blank() {
            let (ctl_tx, last_msg, _cancel) = spawn_blank_fake_engine("main");
            let resp = handle_blank(&ctl_tx, "main", BlankRequestMode::Hard).await;
            assert!(resp.ok, "hard blank should succeed: {resp:?}");

            tokio::task::yield_now().await;

            let msg = last_msg.lock().unwrap().take();
            assert!(
                matches!(msg, Some(ControlMsg::ForceBlank(ref id)) if id.0 == "main"),
                "expected ControlMsg::ForceBlank(main), got {msg:?}"
            );
        }

        /// `Blank` for an unknown display returns an error and DOES NOT send
        /// any control message.  Pin: a routing fix must not regress this.
        #[tokio::test]
        async fn handle_blank_unknown_display_does_not_send() {
            let (ctl_tx, last_msg, _cancel) = spawn_blank_fake_engine("main");
            let resp = handle_blank(&ctl_tx, "missing", BlankRequestMode::Hard).await;
            assert!(!resp.ok, "unknown display should error");
            assert!(
                resp.error.as_deref().is_some_and(|e| e.contains("missing")),
                "error should name the display, got: {:?}",
                resp.error
            );

            // Give the recorder a chance to swallow a wrongly-sent message.
            tokio::task::yield_now().await;
            assert!(
                last_msg.lock().unwrap().is_none(),
                "must not send any control message for an unknown display"
            );
        }
    }
}
