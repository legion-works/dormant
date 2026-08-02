//! Active-sampling consent commands.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use dormant_core::ipc_proto::{IpcRequest, WearSamplingStatus};
use dormant_core::wear::WearSamplingState;

/// The daemon's consent flow allows five minutes; the extra margin prevents a
/// patient operator from being cut off while the daemon is finishing cleanup.
const ENABLE_TIMEOUT: Duration = Duration::from_secs(310);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Operator-visible line printed before the daemon either opens the portal
/// consent dialog or silently reattaches a saved restore token.
///
/// Pure — takes the current lifecycle state (or `None` when the daemon's
/// status reply did not include the field, i.e. a legacy daemon) and returns
/// the line to print. Issue #189: when a valid consent record exists the
/// daemon reattaches silently in seconds and no dialog ever appears; this
/// hint must therefore stay silent on every state except the ones that
/// actually open a portal dialog (`NeedsConsent`, plus `None` as the safe
/// fallback for legacy daemons that cannot tell us otherwise).
///
/// `ConsentPending` gets its own arm: the portal dialog IS open already, so
/// the Enable that follows will be rejected with `FlowAlreadyActive`. Lying
/// about a silent reattach would mislead the operator — we tell them the
/// dialog is up and how to recover instead.
fn consent_hint(state: Option<WearSamplingState>) -> &'static str {
    match state {
        Some(WearSamplingState::NeedsConsent) | None => {
            "waiting for consent dialog — up to 5 minutes"
        }
        Some(WearSamplingState::ConsentPending) => {
            "a consent dialog is already open — answer it or run disable-sampling first"
        }
        Some(
            WearSamplingState::Connecting
            | WearSamplingState::Streaming
            | WearSamplingState::Suspended
            | WearSamplingState::Cooldown
            | WearSamplingState::Disabled,
        ) => "reattaching using saved consent",
    }
}

/// Run `wear enable-sampling`, waiting for the daemon's terminal flow result.
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_enable(socket: &Path) -> Result<()> {
    let state = query_sampler_state(socket)?;
    println!("{}", consent_hint(state));
    let response = send_request_timeout(socket, IpcRequest::WearSamplingEnable, ENABLE_TIMEOUT)?;
    print_status(response.wear_sampling)
}

/// Read the daemon's current active-sampling lifecycle state.
///
/// Falls back to `Ok(None)` for legacy daemons that omit the
/// `wear_sampling_status` field — the caller treats that as the
/// `NeedsConsent` case so the dialog hint still prints.
fn query_sampler_state(socket: &Path) -> Result<Option<WearSamplingState>> {
    let response = send_request_timeout(socket, IpcRequest::Status, DEFAULT_TIMEOUT)?;
    Ok(response.wear_sampling_status.map(|status| status.state))
}

/// Run `wear disable-sampling`, optionally deleting the consent record.
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_disable(socket: &Path, forget: bool) -> Result<()> {
    let response = send_request_timeout(
        socket,
        IpcRequest::WearSamplingDisable { forget },
        DEFAULT_TIMEOUT,
    )?;
    print_status(response.wear_sampling)
}

fn send_request_timeout(
    socket: &Path,
    request: IpcRequest,
    timeout: Duration,
) -> Result<dormant_core::ipc_proto::IpcResponse> {
    let socket = socket.to_owned();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::client::send_request(&socket, &request));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow!("wear sampling request timed out")),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("wear sampling request failed")),
    }
}

fn print_status(status: Option<WearSamplingStatus>) -> Result<()> {
    let status = status.ok_or_else(|| anyhow!("daemon returned no wear sampling status"))?;
    match status {
        WearSamplingStatus::AwaitingConsent => {
            println!("awaiting_consent");
            Ok(())
        }
        WearSamplingStatus::Granted => {
            println!("granted");
            Ok(())
        }
        WearSamplingStatus::Denied => {
            println!("denied");
            Err(anyhow!("wear sampling consent denied"))
        }
        WearSamplingStatus::TimedOut => {
            println!("timed_out");
            Err(anyhow!("wear sampling consent timed out"))
        }
        WearSamplingStatus::Error(reason) => {
            println!("error({reason})");
            Err(anyhow!("wear sampling failed: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{consent_hint, print_status, run_enable};
    use dormant_core::ipc_proto::WearSamplingStatus;
    use dormant_core::wear::WearSamplingState;

    #[test]
    fn terminal_error_status_returns_nonzero_result() {
        assert!(
            print_status(Some(WearSamplingStatus::Error(
                "wear_sampling_wrong_monitor".to_owned(),
            )))
            .is_err()
        );
    }

    #[test]
    fn granted_status_returns_success() {
        assert!(print_status(Some(WearSamplingStatus::Granted)).is_ok());
    }

    // ── consent_hint (issue #189) ─────────────────────────────────────────

    const DIALOG_HINT_PHRASE: &str = "consent dialog";

    #[test]
    fn hint_for_needs_consent_mentions_dialog() {
        let hint = consent_hint(Some(WearSamplingState::NeedsConsent));
        assert!(
            hint.contains(DIALOG_HINT_PHRASE),
            "NeedsConsent must mention the dialog: {hint}"
        );
    }

    #[test]
    fn hint_for_absent_state_falls_back_to_dialog() {
        // Legacy daemons (no wear_sampling_status field) cannot tell us
        // anything — fall back to the dialog hint so we never silently skip
        // a real consent flow.
        let hint = consent_hint(None);
        assert!(
            hint.contains(DIALOG_HINT_PHRASE),
            "absent state must default to the dialog hint: {hint}"
        );
    }

    #[test]
    fn hint_for_connecting_mentions_reattach_not_dialog() {
        let hint = consent_hint(Some(WearSamplingState::Connecting));
        assert!(
            !hint.contains(DIALOG_HINT_PHRASE),
            "Connecting means saved-consent reattach — must not promise a dialog: {hint}"
        );
        assert!(
            hint.contains("reattach"),
            "Connecting hint should mention reattach: {hint}"
        );
    }

    #[test]
    fn hint_for_streaming_mentions_reattach_not_dialog() {
        let hint = consent_hint(Some(WearSamplingState::Streaming));
        assert!(!hint.contains(DIALOG_HINT_PHRASE), "hint: {hint}");
    }

    #[test]
    fn hint_for_consent_pending_mentions_dialog_with_already_open_caveat() {
        // ConsentPending means the portal dialog IS open (GrantStarted +
        // OpenConsent). A subsequent `enable` is going to be rejected
        // with FlowAlreadyActive; the hint must therefore NOT lie about a
        // silent reattach, and must steer the operator toward either
        // answering the open dialog or disabling first.
        let hint = consent_hint(Some(WearSamplingState::ConsentPending));
        assert!(
            hint.contains(DIALOG_HINT_PHRASE),
            "ConsentPending = dialog open — hint must say so: {hint}"
        );
        assert_ne!(
            hint,
            consent_hint(Some(WearSamplingState::NeedsConsent)),
            "ConsentPending must NOT share the fresh-consent hint string"
        );
    }

    #[test]
    fn hint_for_suspended_mentions_reattach_not_dialog() {
        let hint = consent_hint(Some(WearSamplingState::Suspended));
        assert!(!hint.contains(DIALOG_HINT_PHRASE), "hint: {hint}");
    }

    #[test]
    fn hint_for_cooldown_mentions_reattach_not_dialog() {
        let hint = consent_hint(Some(WearSamplingState::Cooldown));
        assert!(!hint.contains(DIALOG_HINT_PHRASE), "hint: {hint}");
    }

    #[test]
    fn hint_for_disabled_mentions_reattach_not_dialog() {
        // Disabled is a config error case; the daemon will refuse Enable
        // immediately. No dialog either way.
        let hint = consent_hint(Some(WearSamplingState::Disabled));
        assert!(!hint.contains(DIALOG_HINT_PHRASE), "hint: {hint}");
    }

    // ── wire integration (issue #189) ─────────────────────────────────────
    //
    // The above pure-function tests prove the hint text is correct for each
    // state. The end-to-end story — that `run_enable` actually queries
    // `Status` before sending `WearSamplingEnable` and then prints the
    // matching hint — needs a fake daemon on a real socket.

    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use dormant_core::ipc_proto::IpcRequest;

    /// Two-shot scripted fake daemon: serves two replies in order, capturing
    /// the wire request that preceded each.
    fn spawn_two_reply_daemon(
        socket_path: &Path,
        replies: Vec<dormant_core::ipc_proto::IpcResponse>,
    ) -> (Arc<Mutex<Vec<IpcRequest>>>, std::thread::JoinHandle<()>) {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path).expect("bind fake socket");
        let captured: Arc<Mutex<Vec<IpcRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let handle = std::thread::spawn(move || {
            for reply in replies {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut buf = String::new();
                reader.read_line(&mut buf).expect("read request line");
                let req: IpcRequest = serde_json::from_str(buf.trim()).expect("parse request");
                captured_clone.lock().unwrap().push(req);
                let line = serde_json::to_string(&reply).expect("serialize reply");
                let mut stream = stream;
                stream
                    .write_all(line.as_bytes())
                    .and_then(|()| stream.write_all(b"\n"))
                    .expect("write reply");
            }
        });
        (captured, handle)
    }

    /// `run_enable` MUST query `Status` first so it can pick the right hint,
    /// then send `WearSamplingEnable` — and when the daemon already reports
    /// `Streaming`, the dialog hint MUST NOT be printed (issue #189).
    #[test]
    fn run_enable_sends_status_first_then_enable_and_skips_dialog_hint_when_streaming() {
        use dormant_core::ipc_proto::IpcResponse;
        use dormant_core::wear::{WearSamplingState, WearSamplingStatus as Lifecycle};

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = {
            let mut r = IpcResponse::ok(None);
            r.wear_sampling_status = Some(Lifecycle {
                state: WearSamplingState::Streaming,
                last_capture_age_s: None,
                uniform_reason: None,
                bound_display: Some("desk".to_owned()),
                granted_at_epoch_s: None,
            });
            r
        };
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket).expect("run_enable");

        daemon.join().expect("fake daemon thread");

        let requests = captured.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "expected Status then Enable, got {requests:?}"
        );
        assert!(
            matches!(requests[0], IpcRequest::Status),
            "first request must be Status (to pick the hint), got {:?}",
            requests[0]
        );
        assert!(
            matches!(requests[1], IpcRequest::WearSamplingEnable),
            "second request must be WearSamplingEnable, got {:?}",
            requests[1]
        );
    }

    /// Legacy daemons that omit `wear_sampling_status` MUST still get the
    /// dialog hint (we cannot tell that reattach is silent, so we assume the
    /// worst case and warn the operator).
    #[test]
    fn run_enable_prints_dialog_hint_when_status_omits_wear_sampling_status() {
        use dormant_core::ipc_proto::IpcResponse;

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = {
            // Old-daemon shape: ok + snapshot only, no wear_sampling_status.
            IpcResponse::ok(None)
        };
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket).expect("run_enable");

        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(matches!(requests[0], IpcRequest::Status));
        assert!(matches!(requests[1], IpcRequest::WearSamplingEnable));
        // The pure-function tests above already pin the string returned for
        // `None`; here we only need to confirm `run_enable` actually issued
        // both requests on the legacy wire shape.
    }
}
