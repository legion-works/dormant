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
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use dormant_tray::ipc_loop::spawn_event_pump;
use dormantctl::client::{EventShutdown, EventStream};

/// Count the live OS threads in this process.  `/proc/self/task`
/// contains one entry per thread; the OS keeps the directory in sync
/// with thread creation / exit.
fn task_count() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count)
}

/// Poll `pred` until it returns true or `deadline` elapses.
fn wait_for<F: Fn() -> bool>(pred: F, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    pred()
}

#[test]
fn spawn_event_pump_thread_exits_when_tick_shutdown_drops() {
    let baseline = task_count();

    // Real socketpair.  The pump reads from `stream_a`; the test (the
    // "daemon" side) writes from `stream_b`.  `UnixStream::pair()`
    // gives each end its own receive buffer — data written to `b`
    // lands in `a`'s buffer for the pump to read.
    let (stream_a, mut stream_b) = UnixStream::pair().unwrap();

    // Build the EventStream + shutdown handle from `stream_a` BEFORE
    // writing the test event so the pump is set up and blocked on
    // read by the time data arrives.
    let event_stream = EventStream::from_reader(BufReader::new(stream_a.try_clone().unwrap()));
    let shutdown = EventShutdown::from_stream(stream_a.try_clone().unwrap());

    let (mut rx, guard) = spawn_event_pump(event_stream, shutdown);

    // Push one line from `b` so the pump has something to read (then
    // it blocks waiting for the next one, which is what we want to
    // observe).
    stream_b
        .write_all(b"{\"event\":\"config_reloaded\"}\n")
        .unwrap();
    stream_b.flush().unwrap();

    // Drain the one queued event so the pump consumes it and parks
    // inside read_line for the next one.  We poll `try_recv` with a
    // short sleep instead of `recv().await` to avoid any runtime /
    // waker timing surprises across the std::thread ↔ tokio boundary.
    let mut got_event = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ev) => {
                assert!(ev.is_ok(), "queued event should parse cleanly");
                got_event = true;
                break;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("try_recv error: {e}"),
        }
    }
    assert!(got_event, "pump should deliver the queued event within 2s");

    // Pump is now blocked on read_line — one extra thread.
    let during = task_count();
    assert!(
        during > baseline,
        "pump thread did not start: baseline={baseline} during={during}"
    );

    // Simulate `tick()`'s guard dropping (normal return, `?` error,
    // or panic unwind — all paths land here).
    drop(guard);

    // The shutdown fires synchronously in Drop; the pump's read_line
    // returns EOF, the iterator ends, the thread exits.  Poll until
    // thread count returns to baseline.
    let settled = wait_for(|| task_count() <= baseline, Duration::from_secs(2));
    let after = task_count();
    assert!(
        settled,
        "pump thread leaked after shutdown: baseline={baseline} during={during} after={after}"
    );

    // Drain the receiver so the channel teardown doesn't dangle.
    drop(rx);
}

/// The OLD pump wiring (no shutdown handle) leaks the thread on early
/// exit.  This test exercises that exact wiring in isolation to
/// demonstrate the leak the fix prevents — so any future regression
/// that drops the shutdown handle gets caught immediately, even
/// before the integrated `tick()` path runs.
///
/// If this test ever starts passing, the leak is gone (and we can
/// retire the test); it is explicitly a red-guard, not a permanent
/// coverage assertion.
#[test]
fn old_pump_without_shutdown_handle_leaks_thread() {
    let baseline = task_count();

    let (stream_a, mut stream_b) = UnixStream::pair().unwrap();
    let event_stream = EventStream::from_reader(BufReader::new(stream_a.try_clone().unwrap()));

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let _pump = std::thread::spawn(move || {
        let mut stream = event_stream;
        for ev in stream.by_ref() {
            if tx.blocking_send(ev).is_err() {
                break;
            }
        }
    });

    // Drain the one queued event.  Write from `b` so the data lands
    // in `a`'s receive buffer; poll with try_recv to avoid any
    // runtime / waker coupling between the std::thread pump and a
    // tokio runtime.
    stream_b
        .write_all(b"{\"event\":\"config_reloaded\"}\n")
        .unwrap();
    stream_b.flush().unwrap();

    let mut got = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(ev) = rx.try_recv() {
            assert!(ev.is_ok());
            got = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(got, "pump should deliver the queued event");

    // Confirm pump is blocked (one extra thread).
    let during = task_count();
    assert!(
        during > baseline,
        "pump thread did not start: baseline={baseline} during={during}"
    );

    // Simulate early tick() exit: drop rx WITHOUT calling shutdown.
    // Without the shutdown handle the thread is still blocked in
    // read_line and stays alive — that's the leak.
    drop(rx);

    // Give the OS a beat to deliver any pending signals.
    let settled = wait_for(|| task_count() <= baseline, Duration::from_millis(300));
    assert!(
        !settled,
        "pump without shutdown unexpectedly exited — the leak guard is no longer guarding. baseline={baseline} during={during}"
    );

    // Clean up: shut down the FD so the thread can exit before the
    // test process does.  (Without this, the test process would exit
    // and the thread would be killed mid-read.)
    stream_a.shutdown(Shutdown::Both).ok();

    // Now the thread should exit.
    let settled = wait_for(|| task_count() <= baseline, Duration::from_secs(2));
    assert!(
        settled,
        "thread did not exit even after manual FD shutdown: baseline={baseline}"
    );
}
