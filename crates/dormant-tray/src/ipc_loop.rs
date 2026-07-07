//! Reconnecting event-stream reader that drives the tray's shared state.
//!
//! Lives on a dedicated tokio task.  Every loop iteration:
//!
//! 1. Issues an [`IpcRequest::Status`] to fetch the current snapshot.
//! 2. Subscribes to [`IpcRequest::Events`] for live updates.
//! 3. On every received event (or on the initial snapshot), publishes the
//!    fresh snapshot to the tray's shared [`TrayState`].
//!
//! On any I/O / parse failure the loop closes the stream and sleeps with
//! capped exponential backoff (1s..30s, doubling) before retrying.  The
//! tray sees `unreachable = true` between attempts and greys the icon.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::rules::{DaemonEvent, StateSnapshot};
use dormantctl::client;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::state::IconState;
use crate::tray::TrayState;

/// Backoff bounds.  Capped exponential: 1, 2, 4, 8, 16, 30, 30, …
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Drop-guard that fires [`client::EventShutdown::shutdown`] on every
/// exit path so the pump thread's blocked `read_line` returns and the
/// thread exits cleanly.  Holding this guard in `tick` is what stops
/// the per-failure thread leak: without it, every early `?` (failed
/// `fetch_status`, parse error, etc.) would leave the pump thread
/// parked on the FD until the daemon eventually sent something — leaking
/// one thread per reconnect.
pub struct TickShutdown(Option<client::EventShutdown>);

impl TickShutdown {
    fn new(shutdown: client::EventShutdown) -> Self {
        Self(Some(shutdown))
    }
}

impl Drop for TickShutdown {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            // Best-effort: the kernel-level shutdown(Both) makes the
            // blocked read on the original FD return EOF/Err.  An error
            // here (e.g. already-closed FD) is fine — the goal is to
            // unblock the read, not to perform a clean half-close.
            let _ = s.shutdown();
        }
    }
}

/// Spawn the synchronous event-iterator pump on a dedicated OS thread
/// and return a tokio-side receiver plus the [`TickShutdown`] guard.
///
/// `exited` is flipped to `true` immediately before the pump thread
/// returns, giving tests a deterministic, cross-environment handle on
/// thread lifetime (the OS-thread-count trick is racy under CI load and
/// gets this exact class wrong — the memory-1824 leak-guard regression
/// was red on the runner because of it).
///
/// Exposed for tests; the production path uses `tick` which calls
/// this with the result of [`client::connect_events`].
#[must_use]
pub fn spawn_event_pump(
    stream: client::EventStream,
    shutdown: client::EventShutdown,
    exited: Arc<AtomicBool>,
) -> (
    tokio::sync::mpsc::Receiver<Result<DaemonEvent>>,
    TickShutdown,
) {
    let (tx, rx) = mpsc::channel::<Result<DaemonEvent>>(32);
    std::thread::spawn(move || {
        let mut stream = stream;
        for ev in stream.by_ref() {
            if tx.blocking_send(ev).is_err() {
                break; // receiver dropped
            }
        }
        exited.store(true, Ordering::SeqCst);
    });
    (rx, TickShutdown::new(shutdown))
}

/// Drive the IPC loop until `cancel` is triggered.
///
/// Blocks (returns only on `cancel.cancel()`).  Spawn this on a
/// dedicated task; the tray binary awaits it from `main`.
pub async fn run(
    socket_path: PathBuf,
    state: Arc<Mutex<TrayState>>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let tick_result = tick(&socket_path, &state, &cancel).await;
        match tick_result {
            Ok(TickExit::Cancelled) => return,
            Ok(TickExit::Closed) => {
                // Daemon closed the stream cleanly — back off and retry.
            }
            Err(e) => {
                warn!(error = %e, "ipc tick failed; will retry");
            }
        }
        {
            let mut s = state.lock().await;
            if !s.unreachable {
                warn!(?backoff, "ipc stream lost; entering reconnect loop");
            }
            s.unreachable = true;
            s.snapshot = None;
            s.icon_state = IconState::Unreachable;
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Why one `tick` returned.
enum TickExit {
    /// The cancel token fired.
    Cancelled,
    /// The daemon closed the event stream (EOF).
    Closed,
}

/// One connection's lifetime: fetch the initial snapshot, then loop on
/// the event stream until it ends or `cancel` fires.
async fn tick(
    socket_path: &Path,
    state: &Arc<Mutex<TrayState>>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<TickExit> {
    let snapshot = fetch_status(socket_path).await.context("initial Status")?;
    publish_snapshot(state, snapshot).await;
    info!("ipc: connected, snapshot published");

    // Subscribe to events.  Wrap the synchronous iterator in a
    // blocking-thread → tokio-mpsc pump so we can `select!` on it.
    // The `TickShutdown` guard calls `shutdown(Both)` on the cloned
    // socket FD on every exit path (normal return, `?` error, panic
    // unwind) — that unblocks the pump thread's `read_line` so it
    // exits instead of leaking across reconnects.
    let (stream, shutdown) = client::connect_events(socket_path).context("subscribe Events")?;
    // Per-tick exit flag — tests assert on this; production drops the
    // handle on every iteration so the value is meaningless here, but
    // allocating once per tick is fine (it's a single AtomicBool).
    let pump_exited = Arc::new(AtomicBool::new(false));
    let (mut rx, _shutdown_guard) = spawn_event_pump(stream, shutdown, pump_exited);

    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(TickExit::Cancelled),
            ev = rx.recv() => {
                match ev {
                    Some(Ok(_event)) => {
                        // Refetch the snapshot — events are notifications
                        // but the snapshot is the truth.
                        match fetch_status(socket_path).await {
                            Ok(snap) => publish_snapshot(state, snap).await,
                            Err(e) => {
                                debug!(error = %e, "post-event Status failed");
                                return Err(e);
                            }
                        }
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "event stream error");
                        return Err(e);
                    }
                    None => {
                        info!("event stream closed by daemon");
                        return Ok(TickExit::Closed);
                    }
                }
            }
        }
    }
}

async fn fetch_status(socket_path: &Path) -> Result<StateSnapshot> {
    // Wrap the synchronous `send_request` in a blocking task so the
    // tokio runtime stays responsive while the Unix I/O completes.
    let path = socket_path.to_path_buf();
    let resp =
        tokio::task::spawn_blocking(move || client::send_request(&path, &IpcRequest::Status))
            .await
            .context("status join")??;
    if !resp.ok {
        anyhow::bail!(
            "daemon returned error on Status: {}",
            resp.error.as_deref().unwrap_or("unknown")
        );
    }
    resp.snapshot
        .ok_or_else(|| anyhow::anyhow!("daemon returned no snapshot"))
}

async fn publish_snapshot(state: &Arc<Mutex<TrayState>>, snap: StateSnapshot) {
    let new_icon_state = if state.lock().await.unreachable {
        IconState::Unreachable
    } else {
        crate::state::derive_icon_state(&snap)
    };
    let mut s = state.lock().await;
    s.snapshot = Some(snap);
    s.unreachable = false;
    s.icon_state = new_icon_state;
}
