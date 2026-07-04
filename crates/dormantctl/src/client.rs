//! Unix socket client for communicating with `dormantd`.
//!
//! Connects to the daemon's Unix domain socket, sends a single JSON
//! [`IpcRequest`], and reads the response (or event stream).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result};
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
use dormant_core::rules::DaemonEvent;

/// Connect to the daemon's socket and send one request, returning the
/// response.
///
/// # Errors
///
/// - Connection refused / file not found → friendly error with exit-code hint.
/// - I/O or JSON errors.
pub fn send_request(socket_path: &Path, request: &IpcRequest) -> Result<IpcResponse> {
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

/// Connect to the daemon's event stream: send an `Events` request and return
/// an iterator over [`DaemonEvent`] JSON lines.
///
/// # Errors
///
/// - Connection refused / file not found → friendly error.
/// - I/O or JSON errors on the initial response.
pub fn connect_events(socket_path: &Path) -> Result<EventStream> {
    let mut stream = connect(socket_path)?;
    let request = IpcRequest::Events;
    let line = serde_json::to_string(&request)?;
    writeln!(stream, "{line}")?;
    stream.flush()?;

    Ok(EventStream {
        reader: BufReader::new(stream),
    })
}

/// An iterator over [`DaemonEvent`] JSON lines from the event stream.
pub struct EventStream {
    reader: BufReader<UnixStream>,
}

impl Iterator for EventStream {
    type Item = Result<DaemonEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None, // EOF
                Ok(_) => {
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
fn connect(socket_path: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket_path).with_context(|| {
        format!(
            "daemon not running at '{}'?\n\
             Start dormantd first, or check the socket path with --socket",
            socket_path.display(),
        )
    })
}
