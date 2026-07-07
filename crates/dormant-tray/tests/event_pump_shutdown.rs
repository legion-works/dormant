//! Regression test for the event-stream pump thread leak (M2).
//!
//! On every iteration of the reconnect loop the tray spawns an OS
//! thread that blocks reading from the daemon's Unix-stream socket.
//! Without a shutdown handle, a `tick()` that exits early (failed
//! `fetch_status`, JSON parse error, etc.) would leave the thread
//! parked on the FD — one leaked thread per reconnect.  This test
//! pins the fix: dropping the [`TickShutdown`] guard fires
//! `shutdown(Both)` on a cloned FD so the blocked read returns EOF,
//! the iterator ends, and the thread exits cleanly.

#![cfg(target_os = "linux")]

use std::io::{BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dormant_tray::ipc_loop::spawn_event_pump;
use dormantctl::client::{EventShutdown, EventStream};

/// Bounded poll interval.  10 ms is short enough that 2 s timeouts feel
/// instant in the green case and long enough that we don't burn CPU.
const POLL: Duration = Duration::from_millis(10);

/// Run `pred` until it returns `true` or `deadline` elapses.  Returns
/// `Some(elapsed)` on success, `None` on timeout.
fn wait_for_true<F: FnMut() -> bool>(mut pred: F, deadline: Duration) -> Option<Duration> {
    let start = Instant::now();
    loop {
        if pred() {
            return Some(start.elapsed());
        }
        if start.elapsed() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Pump a single canonical JSON event from `stream_b` into the pump's
/// socket (`stream_a`) and drain it off `rx`.  Both halves of the
/// pair stay alive afterward so the test can decide whether to hold,
/// shutdown, or drop them.
fn pump_one_event(
    stream_b: &mut std::os::unix::net::UnixStream,
    rx: &mut tokio::sync::mpsc::Receiver<anyhow::Result<dormant_core::rules::DaemonEvent>>,
) {
    stream_b
        .write_all(b"{\"event\":\"config_reloaded\"}\n")
        .unwrap();
    stream_b.flush().unwrap();

    let got = wait_for_true(
        || matches!(rx.try_recv(), Ok(Ok(_))),
        Duration::from_secs(2),
    );
    assert!(
        got.is_some(),
        "pump should deliver the queued event within 2 s"
    );
}

#[test]
fn spawn_event_pump_thread_exits_when_tick_shutdown_drops() {
    let (stream_a, mut stream_b) = std::os::unix::net::UnixStream::pair().unwrap();
    let event_stream = EventStream::from_reader(BufReader::new(stream_a.try_clone().unwrap()));
    let shutdown = EventShutdown::from_stream(stream_a.try_clone().unwrap());

    let exited = Arc::new(AtomicBool::new(false));
    let (mut rx, guard) = spawn_event_pump(event_stream, shutdown, exited.clone());

    pump_one_event(&mut stream_b, &mut rx);

    // Pump is now blocked in read_line on the next event.  Drop the
    // shutdown guard — guard fires shutdown(Both) on the cloned FD,
    // the iterator ends, the thread exits and flips `exited`.
    drop(guard);

    // Bounded poll for the flag.
    let elapsed = wait_for_true(|| exited.load(Ordering::SeqCst), Duration::from_secs(2));
    assert!(
        elapsed.is_some(),
        "pump thread did not exit within 2 s after shutdown drop — guard is broken"
    );

    // Drain the receiver so the channel teardown doesn't dangle.
    drop(rx);
}

/// The "without shutdown" wiring must keep the pump thread blocked
/// forever — that's the bug `TickShutdown` exists to fix.  If this
/// assertion ever starts failing, either the inline loop is exiting on
/// a path that should require a shutdown, or the test's environment
/// is closing the peer FD behind our back.
///
/// Determinism note: we never count OS threads (the `task_count`
/// approach was the old implementation and red on the CI runner —
/// memory-1824 leak-guard regression).  We hold `stream_b` alive
/// for the entire wait so `read_line` blocks rather than EOFs.
#[test]
fn old_pump_without_shutdown_handle_keeps_pump_blocked() {
    let (stream_a, mut stream_b) = std::os::unix::net::UnixStream::pair().unwrap();
    let event_stream = EventStream::from_reader(BufReader::new(stream_a.try_clone().unwrap()));

    let exited = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let pump_exited = exited.clone();
    let _pump = std::thread::spawn(move || {
        let mut stream = event_stream;
        for ev in stream.by_ref() {
            if tx.blocking_send(ev).is_err() {
                break; // receiver dropped
            }
        }
        pump_exited.store(true, Ordering::SeqCst);
    });

    // Drain one event so we know the pump is alive and parked in
    // read_line (not still warming up).
    pump_one_event(&mut stream_b, &mut rx);

    // Now: hold the peer socket and the receiver alive.  No event is
    // ever sent, no shutdown handle exists.  The pump MUST remain
    // blocked for the full wait — any earlier exit means the leak
    // guard is no longer guarding.
    let peer_held = stream_b.try_clone().unwrap();
    // Receiver and pump handle stay in scope for the full wait so
    // `blocking_send` and the read loop can't exit early.  They're
    // explicitly dropped in the cleanup block to release the pump
    // thread before the test process terminates.
    let rx_held = rx;

    let unexpectedly_exited =
        wait_for_true(|| exited.load(Ordering::SeqCst), Duration::from_secs(2));
    assert!(
        unexpectedly_exited.is_none(),
        "pump without shutdown unexpectedly exited — the leak guard is no longer guarding"
    );

    // Cleanup: drop every reference to the peer socket so the kernel
    // closes that end of the pair and the blocked read returns EOF.
    // `drop(peer_held)` alone is not enough — the original `stream_b`
    // (and `stream_a`, whose clones the pump holds) are still alive
    // and hold the peer end open until end-of-scope.  We drop them
    // explicitly here so the pump can wind down before the test
    // process terminates.
    drop(peer_held);
    drop(rx_held);
    drop(stream_b);
    drop(stream_a);

    let cleanup_exited = wait_for_true(|| exited.load(Ordering::SeqCst), Duration::from_secs(2));
    assert!(
        cleanup_exited.is_some(),
        "pump failed to exit even after peer FD shutdown — stuck"
    );
}
