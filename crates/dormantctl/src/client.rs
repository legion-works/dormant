//! Unix socket client for communicating with `dormantd`.
//!
//! Connects to the daemon's Unix domain socket, sends a single JSON
//! [`IpcRequest`], and reads the response (or event stream).
//!
//! On non-Unix platforms all functions return a clear error — IPC is
//! Unix-only in this release (Windows native support is M3).

#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
use dormant_core::rules::DaemonEvent;
#[cfg(unix)]
use std::io::BufReader;
#[cfg(unix)]
use std::time::Duration;

/// Maximum line length for IPC frames (1 MB).  Must match the server's limit.
#[cfg(unix)]
const MAX_LINE_BYTES: usize = 1_048_576;

/// Maximum wait for the daemon's per-connection event-stream readiness frame.
#[cfg(unix)]
const EVENTS_READY_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum response wait for ordinary daemon control requests.
#[cfg(unix)]
const IPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// `Exercise` itself permits the daemon twenty seconds of hardware work.
#[cfg(unix)]
const EXERCISE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(25);

/// Connect to the daemon's socket and send one request, returning the
/// response.
///
/// # Errors
///
/// - Connection refused / file not found → friendly error with exit-code hint.
/// - I/O or JSON errors.
/// - On non-Unix platforms, always returns an error.
pub fn send_request(socket_path: &Path, request: &IpcRequest) -> Result<IpcResponse> {
    #[cfg(unix)]
    {
        use std::io::Write;

        let mut stream = connect(socket_path)?;
        let line = serde_json::to_string(request)?;
        writeln!(stream, "{line}")?;
        stream.flush()?;

        read_response(&stream, request)
    }
    #[cfg(not(unix))]
    {
        let _ = (socket_path, request);
        anyhow::bail!(
            "{}: IPC is only supported on Unix platforms in this release",
            dormant_core::error::E_IPC
        );
    }
}

/// The outcome of a typed IPC round-trip — distinguishes a connect-time
/// failure (the daemon socket is not listening) from a post-connect
/// error (the daemon accepted then dropped / garbled the response).
///
/// Issue #202 makes the distinction load-bearing: the cold offline
/// `probe_all_offline` is only safe to run on a connect failure
/// (daemon is not up — the serial port is not held by anyone). A
/// post-connect failure means the daemon IS up and likely owns the
/// port; reopening it for the cold probe set re-introduces the
/// frame-steal the fix targets.
#[derive(Debug)]
pub enum IpcSendOutcome {
    /// `connect(2)` failed — the daemon is not running on this socket.
    /// The only case the plan permits the cold offline probe set to
    /// run in.
    ConnectFailed(anyhow::Error),
    /// The connect succeeded but the round-trip did not (write failed,
    /// read EOF, malformed JSON, non-Unix platform). The daemon is
    /// reachable; its state is authoritative. Do NOT fall back to
    /// offline — respect the daemon's reachability.
    PostConnectError(anyhow::Error),
    /// The round-trip succeeded; the daemon's response is the third
    /// variant's payload. Boxed to keep the enum small (`IpcResponse`
    /// is hundreds of bytes once every optional report is in scope).
    Ok(Box<IpcResponse>),
}

/// `send_request` with the connect error classified separately. Lets
/// callers branch on `ConnectFailed` vs `PostConnectError` without
/// re-implementing the connect/serialize/send/recv/parse protocol.
///
/// # Panics
///
/// Panics if `serde_json::to_string(request)` itself fails — that
/// can only happen for a `serde_json::ser::Error` (recursion limit,
/// non-string map keys, etc.). `IpcRequest` is a flat enum of
/// trivially-serializable variants so this is unreachable in practice;
/// the `expect` is a belt-and-braces guard, not a normal failure mode.
#[must_use = "the typed outcome must be inspected; an Ok variant means a successful round-trip, a PostConnectError must be surfaced to the operator, only ConnectFailed is safe to fall back from"]
pub fn send_request_typed(socket_path: &Path, request: &IpcRequest) -> IpcSendOutcome {
    #[cfg(unix)]
    {
        use std::io::Write;

        let mut stream = match connect(socket_path) {
            Ok(s) => s,
            Err(e) => return IpcSendOutcome::ConnectFailed(e),
        };
        if let Err(e) = (|| -> std::io::Result<()> {
            let line = serde_json::to_string(request).expect("IpcRequest is always serializable");
            writeln!(stream, "{line}")?;
            stream.flush()?;
            Ok(())
        })() {
            return IpcSendOutcome::PostConnectError(
                anyhow::Error::from(e).context("write doctor request to daemon"),
            );
        }

        match read_response(&stream, request) {
            Ok(resp) => IpcSendOutcome::Ok(Box::new(resp)),
            Err(error) => IpcSendOutcome::PostConnectError(error),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (socket_path, request);
        IpcSendOutcome::PostConnectError(anyhow::anyhow!(
            "{}: IPC is only supported on Unix platforms in this release",
            dormant_core::error::E_IPC
        ))
    }
}

/// Connect to the daemon's event stream: send an `Events` request and
/// return an iterator over [`DaemonEvent`] JSON lines plus a shutdown
/// handle.
///
/// The returned [`EventShutdown`] holds a clone of the underlying
/// Unix-stream FD.  Callers that want to abort the blocking read on the
/// stream (for early exit on cancellation or error) should invoke
/// [`EventShutdown::shutdown`] — that fires the FD's `shutdown(Both)`,
/// which makes the in-flight `read_line` return EOF/Err so the iterator
/// ends and the pump thread exits.  Without this, a blocking read on
/// a socket whose remote end has already closed (or whose caller has
/// stopped iterating) leaks the pump thread.
///
/// # Errors
///
/// - Connection refused / file not found → friendly error.
/// - I/O or JSON errors on the initial response.
/// - On non-Unix platforms, always returns an error.
pub fn connect_events(socket_path: &Path) -> Result<(EventStream, EventShutdown)> {
    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::io::AsRawFd;

        let mut stream = connect(socket_path)?;
        // Keep a clone of the FD just so we can shut the read direction
        // down on early exit — see the doc above.
        let shutdown_fd = stream.try_clone()?;
        let request = IpcRequest::Events;
        let line = serde_json::to_string(&request)?;
        writeln!(stream, "{line}")?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let fd = reader.get_ref().as_raw_fd();
        let timeout_ms = i32::try_from(EVENTS_READY_TIMEOUT.as_millis()).unwrap_or(i32::MAX);
        let readable = loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pfd is a valid, initialized single pollfd for the duration of the call.
            let rc = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
            if rc < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("poll event stream readiness");
            }
            break rc > 0;
        };
        // poll(2) guarantees bytes are readable, not that a full line has arrived;
        // acceptable for this trusted, newline-framed local daemon protocol.
        let pending = if readable {
            let mut readiness_line = String::new();
            match reader.read_line(&mut readiness_line) {
                Ok(0) => None,
                Ok(_) => serde_json::from_str(readiness_line.trim())
                    .map(|event| match event {
                        DaemonEvent::Subscribed => None,
                        event => Some(event),
                    })
                    .context("parse event stream readiness")?,
                Err(error) => return Err(error.into()),
            }
        } else {
            None
        };

        Ok((
            EventStream { reader, pending },
            EventShutdown {
                stream: shutdown_fd,
            },
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = socket_path;
        anyhow::bail!(
            "{}: IPC is only supported on Unix platforms in this release",
            dormant_core::error::E_IPC
        );
    }
}

/// A handle that can abort an in-flight event-stream read.
///
/// Constructed by [`connect_events`]; holds a clone of the underlying
/// Unix-stream FD.  Call [`EventShutdown::shutdown`] from a Drop guard
/// or cancellation path so the blocked `read_line` on the main stream
/// returns immediately and the pump thread exits cleanly.
#[cfg(unix)]
pub struct EventShutdown {
    /// Clone of the event-stream's Unix FD.  Held open so we can call
    /// `shutdown(Both)` on it; the kernel-level shutdown propagates to
    /// the original FD held by the iterator.
    stream: UnixStream,
}

#[cfg(unix)]
impl EventShutdown {
    /// Build an `EventShutdown` from an existing `UnixStream` clone.
    ///
    /// Useful for tests that drive the iterator against a
    /// `UnixStream::pair()` and need to construct both halves
    /// manually.  Production code uses [`connect_events`].
    #[must_use]
    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Shutdown both directions of the underlying socket.  After this
    /// returns, any blocked read on the event-stream iterator will
    /// return `Ok(0)` (EOF) — unblocking the pump thread.
    ///
    /// # Errors
    ///
    /// Returns the same I/O errors as
    /// [`std::os::unix::net::UnixStream::shutdown`]: `ENOTCONN` /
    /// `EBADF` if the underlying socket is no longer connected or has
    /// already been shut down.  Callers (the `TickShutdown` drop
    /// guard in `dormant-tray`) treat the result as best-effort —
    /// the goal is to unblock the blocked read on the original FD,
    /// not to perform a clean half-close.
    pub fn shutdown(&self) -> std::io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }
}

#[cfg(not(unix))]
pub struct EventShutdown {
    _marker: std::marker::PhantomData<()>,
}

#[cfg(not(unix))]
impl EventShutdown {
    /// No-op on non-Unix — IPC is not supported there.
    pub fn shutdown(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An iterator over [`DaemonEvent`] JSON lines from the event stream.
///
/// Generic over the reader so tests can drive the parsing/line-length logic
/// against an in-memory buffer instead of a real `UnixStream`; production
/// code always uses the default `R = UnixStream` (via [`EventStream::from_reader`]
/// / [`connect_events`]).
#[cfg(unix)]
pub struct EventStream<R = UnixStream> {
    reader: BufReader<R>,
    pending: Option<DaemonEvent>,
}

#[cfg(not(unix))]
pub struct EventStream {
    _marker: std::marker::PhantomData<()>,
}

#[cfg(unix)]
impl EventStream<UnixStream> {
    /// Build an `EventStream` from a pre-connected `BufReader<UnixStream>`.
    ///
    /// The caller is responsible for writing the `Events` request line
    /// to `reader.get_ref()` before constructing the stream — this
    /// constructor is primarily for tests that drive the iterator
    /// against a `UnixStream::pair()` or similar.
    #[must_use]
    pub fn from_reader(reader: BufReader<UnixStream>) -> Self {
        Self {
            reader,
            pending: None,
        }
    }
}

#[cfg(all(unix, test))]
impl<R: std::io::Read> EventStream<R> {
    /// Build an `EventStream` over any [`std::io::Read`] source — test infra
    /// only. Production callers always go through [`EventStream::from_reader`]
    /// / [`connect_events`], both pinned to `UnixStream`.
    pub(crate) fn from_reader_for_test(reader: BufReader<R>) -> Self {
        Self {
            reader,
            pending: None,
        }
    }
}

#[cfg(unix)]
impl<R: std::io::Read> Iterator for EventStream<R> {
    type Item = Result<DaemonEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::io::{BufRead, Read};

        if let Some(event) = self.pending.take() {
            return Some(Ok(event));
        }

        loop {
            let mut line = String::new();
            // Cap the line buffer so a malicious or broken server cannot
            // cause unbounded memory growth.
            let mut reader = self
                .reader
                .by_ref()
                .take(u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX) + 1);
            match reader.read_line(&mut line) {
                Ok(0) => return None, // EOF
                Ok(n) => {
                    if n > MAX_LINE_BYTES {
                        // Drain the rest of the oversized line.
                        let _ = std::io::copy(
                            &mut self.reader.by_ref().take(u64::MAX),
                            &mut std::io::sink(),
                        );
                        return Some(Err(anyhow::anyhow!(
                            "event line exceeds maximum length of {MAX_LINE_BYTES} bytes"
                        )));
                    }
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        return Some(serde_json::from_str(trimmed).context("parse daemon event"));
                    }
                    // Empty line — continue reading.
                }
                Err(e) => return Some(Err(e.into())),
            }
        }
    }
}

#[cfg(not(unix))]
impl Iterator for EventStream {
    type Item = Result<DaemonEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        let _ = self;
        None
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn spawn_nonresponding_server(
        socket_path: &Path,
        expected: IpcRequest,
    ) -> (thread::JoinHandle<()>, mpsc::Sender<()>) {
        let listener = UnixListener::bind(socket_path).expect("bind fake socket");
        let (release_tx, release_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            assert_eq!(
                serde_json::from_str::<IpcRequest>(line.trim()).expect("parse request"),
                expected
            );
            let _ = release_rx.recv_timeout(Duration::from_secs(11));
        });
        (handle, release_tx)
    }

    #[test]
    fn send_request_times_out_when_daemon_accepts_but_never_replies() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let request = IpcRequest::Status;
        let (server, release) = spawn_nonresponding_server(&socket, request.clone());

        let error = send_request(&socket, &request).expect_err("stalled daemon must time out");

        drop(release);
        server.join().expect("fake daemon must finish");
        assert!(
            format!("{error:#}").contains("timed out"),
            "timeout must be explicit: {error:#}"
        );
    }

    #[test]
    fn send_request_typed_reports_post_connect_timeout_when_daemon_stalls() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let request = IpcRequest::Doctor;
        let (server, release) = spawn_nonresponding_server(&socket, request.clone());

        let outcome = send_request_typed(&socket, &request);

        drop(release);
        server.join().expect("fake daemon must finish");
        match outcome {
            IpcSendOutcome::PostConnectError(error) => assert!(
                format!("{error:#}").contains("timed out"),
                "timeout must be explicit: {error:#}"
            ),
            IpcSendOutcome::ConnectFailed(error) => {
                panic!("accepted connection must not be classified as connect failure: {error:#}")
            }
            IpcSendOutcome::Ok(response) => {
                panic!("stalled daemon must not return a response: {response:?}")
            }
        }
    }

    /// A foreign/unrecognized `"event"` tag must deserialize to
    /// `DaemonEvent::Unknown` instead of erroring the iterator — an older
    /// `dormantctl` talking to a newer daemon must keep streaming past
    /// event kinds it doesn't understand yet.
    #[test]
    fn event_stream_yields_ok_unknown_for_foreign_tag() {
        let lines = b"{\"event\":\"from_the_future\",\"x\":1}\n" as &[u8];
        let mut s = EventStream::from_reader_for_test(BufReader::new(lines));
        assert!(matches!(s.next(), Some(Ok(DaemonEvent::Unknown))));
    }

    #[test]
    fn subscribed_sentinel_deserializes_to_known_event() {
        let event: DaemonEvent = serde_json::from_str("{\"event\":\"subscribed\"}").unwrap();
        assert!(matches!(event, DaemonEvent::Subscribed));
    }

    #[test]
    fn future_event_tag_deserializes_to_unknown() {
        let event: DaemonEvent = serde_json::from_str("{\"event\":\"some_future_tag\"}").unwrap();
        assert!(matches!(event, DaemonEvent::Unknown));
    }

    // ── Fix C (#138): exit code stability ─────────────────────────────

    /// `check_response` on a non-ok `IpcResponse` must return `Err` so the
    /// CLI exits non-zero.  This is the client-side half of the exit-code
    /// contract; the server-side asserts the mapping.
    #[test]
    fn check_response_errors_on_non_ok_response() {
        let resp = IpcResponse {
            ok: false,
            error: Some("write failed: E_DISPLAY_IO".to_string()),
            snapshot: None,
            doctor_report: None,
            emergency_report: None,
            exercise_report: None,
            wear_sampling: None,
            wear_sampling_status: None,
            wear_sampling_statuses: None,
            switch_outcome: None,
        };
        let result = check_response(&resp);
        assert!(result.is_err(), "non-ok response must produce an error");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("write failed"),
            "error must preserve the failure detail, got: {err}"
        );
    }

    #[test]
    fn check_response_ok_on_success_response() {
        let resp = IpcResponse {
            ok: true,
            error: None,
            snapshot: None,
            doctor_report: None,
            emergency_report: None,
            exercise_report: None,
            wear_sampling: None,
            wear_sampling_status: None,
            wear_sampling_statuses: None,
            switch_outcome: None,
        };
        assert!(check_response(&resp).is_ok());
    }

    #[test]
    fn event_stream_yields_pending_event_before_reader() {
        let lines = b"{\"event\":\"from_the_future\"}\n" as &[u8];
        let mut stream = EventStream {
            reader: BufReader::new(lines),
            pending: Some(DaemonEvent::ConfigReloaded),
        };

        assert!(matches!(
            stream.next(),
            Some(Ok(DaemonEvent::ConfigReloaded))
        ));
        assert!(matches!(stream.next(), Some(Ok(DaemonEvent::Unknown))));
    }
}

/// Check an [`IpcResponse`] for success, printing "ok" or returning an error.
///
/// # Errors
///
/// Returns an error with the daemon's error message if `resp.ok` is false.
pub fn check_response(resp: &IpcResponse) -> Result<()> {
    if resp.ok {
        println!("ok");
        Ok(())
    } else {
        anyhow::bail!("{}", resp.error.as_deref().unwrap_or("unknown error"))
    }
}

#[cfg(unix)]
fn read_response(stream: &UnixStream, request: &IpcRequest) -> Result<IpcResponse> {
    use std::io::BufRead;

    let timeout = response_timeout(request);
    stream
        .set_read_timeout(Some(timeout))
        .context("set daemon response timeout")?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    match reader.read_line(&mut response_line) {
        Ok(0) => anyhow::bail!("daemon closed connection before sending a response"),
        Ok(_) => serde_json::from_str(response_line.trim()).context("parse daemon response"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            anyhow::bail!(
                "daemon response timed out after {} seconds",
                timeout.as_secs()
            )
        }
        Err(error) => Err(error).context("read response from daemon"),
    }
}

#[cfg(unix)]
fn response_timeout(request: &IpcRequest) -> Duration {
    match request {
        IpcRequest::Exercise { .. } => EXERCISE_RESPONSE_TIMEOUT,
        _ => IPC_RESPONSE_TIMEOUT,
    }
}

/// Connect to the daemon's Unix socket.
#[cfg(unix)]
fn connect(socket_path: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket_path).with_context(|| {
        format!(
            "daemon not running at '{}'?\n\
             Start dormantd first, or check the socket path with --socket",
            socket_path.display(),
        )
    })
}
