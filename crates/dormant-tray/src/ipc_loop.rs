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

/// Compute the next backoff duration from a tick outcome.
///
/// A tick that successfully connected (published ≥1 snapshot and reached the
/// event loop) resets to [`BACKOFF_MIN`] for a prompt reconnect; cold-connect
/// failures escalate exponentially and are capped at [`BACKOFF_MAX`].
fn next_backoff(current: Duration, connected_this_tick: bool) -> Duration {
    if connected_this_tick {
        BACKOFF_MIN
    } else {
        (current * 2).min(BACKOFF_MAX)
    }
}

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
        let outcome = tick(&socket_path, &state, &cancel).await;
        let connected = match &outcome {
            TickOutcome::Cancelled => return,
            TickOutcome::Closed => {
                backoff = BACKOFF_MIN;
                true
            }
            TickOutcome::Errored { connected, error } => {
                warn!(error = %error, "ipc tick failed; will retry");
                if *connected {
                    backoff = BACKOFF_MIN;
                }
                *connected
            }
        };
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
        backoff = next_backoff(backoff, connected);
    }
}

/// The result of one connection attempt.
#[derive(Debug)]
enum TickOutcome {
    /// The cancel token fired.
    Cancelled,
    /// The daemon closed the event stream cleanly.
    /// Reaching this variant implies the connection was healthy.
    Closed,
    /// An error occurred.  `connected` indicates whether the tick had
    /// already fetched the initial snapshot and entered the event loop
    /// (healthy connection) before the error arrived.
    Errored {
        connected: bool,
        error: anyhow::Error,
    },
}

/// One connection's lifetime: fetch the initial snapshot, then loop on
/// the event stream until it ends or `cancel` fires.
async fn tick(
    socket_path: &Path,
    state: &Arc<Mutex<TrayState>>,
    cancel: &tokio_util::sync::CancellationToken,
) -> TickOutcome {
    let mut connected = false;

    // 1. Fetch initial snapshot.  Failure here is a cold-connect error.
    let snapshot = match fetch_status(socket_path).await {
        Ok(s) => s,
        Err(e) => {
            return TickOutcome::Errored {
                connected,
                error: e.context("initial Status"),
            };
        }
    };
    publish_snapshot(state, snapshot).await;
    info!("ipc: connected, snapshot published");
    connected = true;

    // 2. Subscribe to events.  Connect failure AFTER a successful fetch + publish
    //    is a healthy-tick error — we already proved the socket works.
    let (stream, shutdown) = match client::connect_events(socket_path) {
        Ok(pair) => pair,
        Err(e) => {
            return TickOutcome::Errored {
                connected,
                error: e.context("subscribe Events"),
            };
        }
    };
    // Per-tick exit flag — tests assert on this; production drops the
    // handle on every iteration so the value is meaningless here, but
    // allocating once per tick is fine (it's a single AtomicBool).
    let pump_exited = Arc::new(AtomicBool::new(false));
    let (mut rx, _shutdown_guard) = spawn_event_pump(stream, shutdown, pump_exited);

    // 3. Event loop.
    loop {
        tokio::select! {
            () = cancel.cancelled() => return TickOutcome::Cancelled,
            ev = rx.recv() => {
                match ev {
                    Some(Ok(_event)) => {
                        // Refetch the snapshot — events are notifications
                        // but the snapshot is the truth.
                        match fetch_status(socket_path).await {
                            Ok(snap) => publish_snapshot(state, snap).await,
                            Err(e) => {
                                debug!(error = %e, "post-event Status failed");
                                return TickOutcome::Errored { connected, error: e };
                            }
                        }
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "event stream error");
                        return TickOutcome::Errored { connected, error: e };
                    }
                    None => {
                        info!("event stream closed by daemon");
                        return TickOutcome::Closed;
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- Existing tests (updated to new signature) ---

    #[test]
    fn closed_resets_to_min_even_from_max() {
        let result = next_backoff(BACKOFF_MAX, true);
        assert_eq!(
            result, BACKOFF_MIN,
            "RED: Closed should reset to BACKOFF_MIN, got {result:?}"
        );
    }

    #[test]
    fn error_escalates_and_caps() {
        let result = next_backoff(BACKOFF_MIN, false);
        assert_eq!(result, Duration::from_secs(2), "1s → 2s");

        let result = next_backoff(Duration::from_secs(16), false);
        assert_eq!(result, BACKOFF_MAX, "16s → caps at 30s");

        let result = next_backoff(BACKOFF_MAX, false);
        assert_eq!(result, BACKOFF_MAX, "already at cap stays capped");
    }

    #[test]
    fn sequence_closed_resets_after_errors() {
        // Simulate: Err(1s) → Err(2s) → Closed → next should be BACKOFF_MIN
        let b1 = next_backoff(BACKOFF_MIN, false);
        assert_eq!(b1, Duration::from_secs(2));

        let b2 = next_backoff(b1, false);
        assert_eq!(b2, Duration::from_secs(4));

        // After errors, a clean close must reset to BACKOFF_MIN.
        let b3 = next_backoff(b2, true);
        assert_eq!(
            b3, BACKOFF_MIN,
            "RED: after Err→Err→Closed, backoff should be BACKOFF_MIN, got {b3:?}"
        );
    }

    // --- New tests for the review Should ---

    #[test]
    fn errored_after_connect_resets_to_min() {
        // A healthy-then-Err tick (connected=true) must reset to BACKOFF_MIN.
        let result = next_backoff(BACKOFF_MAX, true);
        assert_eq!(
            result, BACKOFF_MIN,
            "healthy connection resets to BACKOFF_MIN, got {result:?}"
        );
    }

    #[test]
    fn errored_before_connect_escalates() {
        // A cold-connect failure (connected=false) must still escalate.
        let result = next_backoff(Duration::from_secs(4), false);
        assert_eq!(
            result,
            Duration::from_secs(8),
            "cold-connect failure escalates 4s → 8s"
        );
    }

    // --- Wire-tolerance: an unrecognized DaemonEvent tag must not error tick() ---

    /// `tick()`'s event loop never inspects the `DaemonEvent` payload — any
    /// `Ok(_)` off the pump channel (recognized variant or the wire-tolerance
    /// `Unknown` catch-all alike) just triggers a snapshot refetch. This
    /// pins that behavior for the new `Unknown` variant: a fake daemon sends
    /// one foreign-tagged event, and `tick()` must NOT surface it as
    /// `TickOutcome::Errored` — it should refetch cleanly and only end (via
    /// `TickOutcome::Closed`) once the daemon closes the connection, mirroring
    /// a normal recognized-event tick.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick_ignores_unknown_event_without_reconnect() {
        use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dormant.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Fake daemon: services the initial Status, then the Events
        // subscribe (writes one foreign-tagged event line and closes so the
        // pump sees EOF), then the post-event Status refetch. Three
        // sequential connections, exactly what `tick()`'s happy path drives.
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = tokio::io::split(stream);
                let mut reader = TokioBufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let req: IpcRequest = serde_json::from_str(line.trim()).unwrap();
                match req {
                    IpcRequest::Status => {
                        let snap = StateSnapshot {
                            sensors: vec![],
                            zones: vec![],
                            displays: vec![],
                            pending_reload: None,
                        };
                        let json = serde_json::to_string(&IpcResponse::ok(Some(snap))).unwrap();
                        writer.write_all(json.as_bytes()).await.unwrap();
                        writer.write_all(b"\n").await.unwrap();
                        writer.flush().await.unwrap();
                    }
                    IpcRequest::Events => {
                        writer
                            .write_all(b"{\"event\":\"from_the_future\",\"x\":1}\n")
                            .await
                            .unwrap();
                        writer.flush().await.unwrap();
                        // Connection drops at end of loop iteration — the
                        // client sees EOF right after this one event.
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        });

        let state = Arc::new(Mutex::new(TrayState::new(sock.clone())));
        let cancel = tokio_util::sync::CancellationToken::new();

        let outcome = tick(&sock, &state, &cancel).await;

        assert!(
            !matches!(outcome, TickOutcome::Errored { .. }),
            "an unrecognized event tag must not error the tick, got {outcome:?}"
        );
        assert!(
            !state.lock().await.unreachable,
            "state must still read as reachable after tolerating the unknown event"
        );

        server.await.unwrap();
    }
}
