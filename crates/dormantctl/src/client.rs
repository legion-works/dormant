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

use anyhow::{Context, Result};
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
use dormant_core::rules::DaemonEvent;
#[cfg(unix)]
use std::io::BufReader;

/// Maximum line length for IPC frames (1 MB).  Must match the server's limit.
const MAX_LINE_BYTES: usize = 1_048_576;

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
        use std::io::{BufRead, BufReader, Write};

        let mut stream = connect(socket_path)?;
        let line = serde_json::to_string(request)?;
        writeln!(stream, "{line}")?;
        stream.flush()?;

        let mut reader = BufReader::new(&stream);
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .context("read response from daemon")?;

        let resp: IpcResponse =
            serde_json::from_str(response_line.trim()).context("parse daemon response")?;
        Ok(resp)
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
        use std::io::{BufReader, Write};

        let mut stream = connect(socket_path)?;
        // Keep a clone of the FD just so we can shut the read direction
        // down on early exit — see the doc above.
        let shutdown_fd = stream.try_clone()?;
        let request = IpcRequest::Events;
        let line = serde_json::to_string(&request)?;
        writeln!(stream, "{line}")?;
        stream.flush()?;

        Ok((
            EventStream {
                reader: BufReader::new(stream),
            },
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
pub struct EventStream {
    #[cfg(unix)]
    reader: BufReader<UnixStream>,
    #[cfg(not(unix))]
    _marker: std::marker::PhantomData<()>,
}

#[cfg(unix)]
impl EventStream {
    /// Build an `EventStream` from a pre-connected `BufReader<UnixStream>`.
    ///
    /// The caller is responsible for writing the `Events` request line
    /// to `reader.get_ref()` before constructing the stream — this
    /// constructor is primarily for tests that drive the iterator
    /// against a `UnixStream::pair()` or similar.
    #[must_use]
    pub fn from_reader(reader: BufReader<UnixStream>) -> Self {
        Self { reader }
    }
}

impl Iterator for EventStream {
    type Item = Result<DaemonEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        #[cfg(unix)]
        {
            use std::io::{BufRead, Read};

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
                            return Some(
                                serde_json::from_str(trimmed).context("parse daemon event"),
                            );
                        }
                        // Empty line — continue reading.
                    }
                    Err(e) => return Some(Err(e.into())),
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            None
        }
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
