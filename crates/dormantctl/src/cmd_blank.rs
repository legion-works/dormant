//! `dormantctl blank` / `dormantctl wake` — blank / wake a display.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use dormant_core::ipc_proto::{BlankRequestMode, IpcRequest};

use dormantctl::client;

/// Run the `blank` command.
///
/// Issue #124 split the blank policy:
/// - `hard=false` (the default) sends [`BlankRequestMode::Soft`] and the
///   daemon walks the configured render/stage/controller ladder from its
///   first stage — no prompt.
/// - `hard=true, yes=false` sends [`BlankRequestMode::Hard`] only after
///   prompting on stdin (TTY required).  Prints the display name and a
///   danger statement, then waits for `y`/`Y` on a single line.
/// - `hard=true, yes=true` sends [`BlankRequestMode::Hard`] with no prompt
///   (CI / scripts).
///
/// # Errors
///
/// Propagates connection and I/O errors, refusal to proceed from a
/// non-TTY stdin, and an unconfirmed prompt.
pub fn run_blank(socket_path: &Path, display: &str, hard: bool, yes: bool) -> Result<()> {
    let mode = if hard {
        BlankRequestMode::Hard
    } else {
        BlankRequestMode::Soft
    };
    if hard && !yes {
        confirm_hard_blank(display)?;
    }
    let resp = client::send_request(
        socket_path,
        &IpcRequest::Blank {
            display: display.to_string(),
            mode,
        },
    )?;
    client::check_response(&resp)
}

/// Prompt the operator on stderr for `y`/`Y` confirmation.
///
/// Refuses to run when stdin is not a TTY (so an unattended script cannot
/// accidentally confirm by piping `yes` into the CLI).  Returns `Err`
/// when the user does not type `y` or `Y` on the first line.
fn confirm_hard_blank(display: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "refusing to hard-blank '{display}' on a non-interactive stdin (no --yes flag given). \
             Re-run with --hard --yes to bypass the prompt in scripts/CI."
        );
    }
    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "About to HARD blank '{display}' \u{2014} this will power off the panel."
    )?;
    writeln!(
        stderr,
        "Shared panels will be affected on every connected machine."
    )?;
    write!(stderr, "Type 'y' or 'Y' and press Enter to confirm: ")?;
    stderr.flush()?;

    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    match line.trim() {
        "y" | "Y" => Ok(()),
        _ => Err(anyhow!(
            "hard blank on '{display}' cancelled (expected 'y' or 'Y', got {:?})",
            line.trim()
        )),
    }
}

/// Run the `wake` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run_wake(socket_path: &Path, display: &str) -> Result<()> {
    let resp = client::send_request(
        socket_path,
        &IpcRequest::Wake {
            display: display.to_string(),
        },
    )?;
    client::check_response(&resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::ipc_proto::IpcResponse;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Capture the wire request received by a single-shot fake daemon so a
    /// test can assert what `dormantctl blank` actually sent.  The listener
    /// accepts exactly one connection, reads one JSON line, replies with a
    /// canned `IpcResponse::ok`, and exits.
    fn spawn_fake_daemon(
        socket_path: &Path,
        captured: Arc<Mutex<Option<IpcRequest>>>,
    ) -> thread::JoinHandle<()> {
        // Make sure the path is free; ignore NotFound.
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path).expect("bind fake socket");
        thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (stream, _) = listener.accept().expect("accept");
            // Use `read_line` (not `read_to_string`) so we return as soon
            // as the request's trailing newline arrives — `read_to_string`
            // would block until the client closes the socket, deadlocking
            // the test.
            let mut reader = std::io::BufReader::new(stream);
            let mut buf = String::new();
            reader.read_line(&mut buf).expect("read request line");
            let req: IpcRequest = serde_json::from_str(buf.trim()).expect("parse request");
            *captured.lock().unwrap() = Some(req);
            // Reply with a success response so `check_response` is happy.
            let resp = IpcResponse::ok(None);
            let line = serde_json::to_string(&resp).expect("serialize response");
            let mut stream = reader.into_inner();
            stream
                .write_all(line.as_bytes())
                .and_then(|()| stream.write_all(b"\n"))
                .expect("write response");
        })
    }

    /// Soft (default) `dormantctl blank` MUST serialize the request with
    /// no `mode` field — the legacy wire shape the new daemons still
    /// accept and that legacy daemons treat as `Soft`.  This is the
    /// issue-#124 safety default.
    #[test]
    fn run_blank_default_soft_sends_legacy_no_mode_frame() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let captured = Arc::new(Mutex::new(None::<IpcRequest>));
        let handle = spawn_fake_daemon(&socket, captured.clone());

        run_blank(&socket, "main", /* hard */ false, /* yes */ false).expect("run_blank");

        handle.join().expect("fake daemon thread");

        let req = captured.lock().unwrap().clone().expect("captured request");
        match req {
            IpcRequest::Blank { display, mode } => {
                assert_eq!(display, "main");
                assert_eq!(
                    mode,
                    BlankRequestMode::Soft,
                    "default mode must be Soft (legacy no-mode wire shape)"
                );
            }
            other => panic!("expected Blank, got {other:?}"),
        }
        let raw = serde_json::to_string(&captured.lock().unwrap().clone().unwrap()).unwrap();
        assert!(
            !raw.contains("\"mode\""),
            "soft frame must elide the mode field, got: {raw}"
        );
    }

    /// `dormantctl blank --hard --yes` MUST serialize the request with
    /// `mode: "hard"` and must NOT prompt on stdin (the bypass path).
    #[test]
    fn run_blank_hard_with_yes_sends_hard_mode() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let captured = Arc::new(Mutex::new(None::<IpcRequest>));
        let handle = spawn_fake_daemon(&socket, captured.clone());

        run_blank(&socket, "main", /* hard */ true, /* yes */ true).expect("run_blank");

        handle.join().expect("fake daemon thread");

        let req = captured.lock().unwrap().clone().expect("captured request");
        match req {
            IpcRequest::Blank { display, mode } => {
                assert_eq!(display, "main");
                assert_eq!(mode, BlankRequestMode::Hard);
            }
            other => panic!("expected Blank, got {other:?}"),
        }
    }

    /// `dormantctl blank --hard` from a non-interactive stdin MUST refuse to
    /// proceed — so an unattended script cannot accidentally confirm by
    /// piping `yes` into the CLI.  `cargo test` runs the tests with stdin
    /// not attached to a TTY (it inherits the runner's non-TTY stdin), so
    /// this test exercises the refusal branch directly.
    #[test]
    fn confirm_hard_blank_refuses_non_tty() {
        // The check is `std::io::stdin().is_terminal()` — the production
        // path.  We exercise the same code by calling `confirm_hard_blank`
        // and asserting the error when stdin is not a TTY.
        let result = confirm_hard_blank("main");
        if std::io::stdin().is_terminal() {
            // If the test happens to be run in a real TTY, the function
            // would block on `read_line` waiting for input — skip in that
            // case.  The CI environment is non-TTY so this branch is
            // taken in practice.
            return;
        }
        let err = result.expect_err("must refuse when stdin is not a TTY");
        let msg = format!("{err}");
        assert!(
            msg.contains("refusing to hard-blank 'main'"),
            "error should explain the refusal: {msg}"
        );
        assert!(
            msg.contains("--yes"),
            "error should direct the user to --yes: {msg}"
        );
    }
}
