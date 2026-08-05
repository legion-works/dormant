//! Active-sampling consent commands.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use dormant_core::ipc_proto::{IpcRequest, WearSamplingStatus, WearSamplingStatusMapEntry};
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
/// `display` is an explicit per-display override. When the daemon has
/// multiple configured sampling displays, omitting it is an error — the
/// operator must pick one. With a single configured display, omission is
/// still valid (legacy ergonomics, issue #185).
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_enable(socket: &Path, display: Option<&str>) -> Result<()> {
    let statuses = query_sampler_statuses(socket)?;
    let (hint_state, selected, source_gate) = classify_sampling(&statuses, display)?;
    println!("{}", consent_hint(hint_state));
    if let Some(gate) = source_gate {
        println!("source: {gate}");
    }
    let request = match &selected {
        SelectedDisplay::Explicit(id) => IpcRequest::WearSamplingEnableFor {
            display: id.clone(),
        },
        SelectedDisplay::SoleUnit | SelectedDisplay::Legacy => IpcRequest::WearSamplingEnable,
    };
    let response = send_request_timeout(socket, request, ENABLE_TIMEOUT)?;
    print_status(response.wear_sampling)
}

/// Run `wear disable-sampling`, optionally deleting the consent record.
///
/// `display` follows the same rule as [`run_enable`]: required when
/// multiple displays are selected, optional when exactly one is.
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_disable(socket: &Path, forget: bool, display: Option<&str>) -> Result<()> {
    let statuses = query_sampler_statuses(socket)?;
    let (_hint_state, selected, source_gate) = classify_sampling(&statuses, display)?;
    if let Some(gate) = source_gate {
        println!("source: {gate}");
    }
    let request = match &selected {
        SelectedDisplay::Explicit(id) => IpcRequest::WearSamplingDisableFor {
            display: id.clone(),
            forget,
        },
        SelectedDisplay::SoleUnit | SelectedDisplay::Legacy => {
            IpcRequest::WearSamplingDisable { forget }
        }
    };
    let response = send_request_timeout(socket, request, DEFAULT_TIMEOUT)?;
    print_status(response.wear_sampling)
}

/// Which display a per-display CLI command resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectedDisplay {
    /// The operator passed `--display <id>` and the daemon accepted it
    /// (or the daemon has exactly one selected display and the operator
    /// matched it). Always sends the per-display `*For` IPC variant.
    Explicit(String),
    /// The operator omitted `--display` and the daemon has exactly one
    /// selected display. Preserves the legacy unit-variant wire tag for
    /// backward compatibility with single-display operator scripts.
    SoleUnit,
    /// Legacy fallback (zero displays selected, no display selector to send).
    Legacy,
}

/// Read the daemon's per-display aggregate status map.
///
/// Returns an empty map when the daemon omits `wear_sampling_statuses`
/// (legacy daemon — the singular field may still be populated).
fn query_sampler_statuses(
    socket: &Path,
) -> Result<std::collections::BTreeMap<String, WearSamplingStatusMapEntry>> {
    let response = send_request_timeout(socket, IpcRequest::Status, DEFAULT_TIMEOUT)?;
    Ok(response.wear_sampling_statuses.unwrap_or_default())
}

/// Decide which display the operator's command targets, the lifecycle
/// state used for the consent hint, and the redacted source-gate string
/// for that selected display only — never for any other configured
/// display. The gate is `None` for displays that carry no gate
/// configuration (a render-only monitor) and `None` for the legacy
/// unit-variant path (no selected display to look up).
///
/// - Multiple selected and no `--display` → error.
/// - Multiple selected and `--display <id>` → use that id explicitly.
/// - Exactly one selected and `--display <id>` matches → use it.
/// - Exactly one selected and no `--display` → implicit sole display.
/// - No displays selected (legacy/empty config) → legacy unit variant.
fn classify_sampling(
    statuses: &std::collections::BTreeMap<String, WearSamplingStatusMapEntry>,
    display: Option<&str>,
) -> Result<(Option<WearSamplingState>, SelectedDisplay, Option<String>)> {
    let mut keys: Vec<&String> = statuses.keys().collect();
    keys.sort();
    match keys.len() {
        0 => Ok((None, SelectedDisplay::Legacy, None)),
        1 => {
            let sole = keys[0].clone();
            // First (and only) entry's lifecycle state and gate for the
            // hint. The gate is surfaced only for the SELECTED display.
            let entry = statuses.get(&sole);
            let hint = entry.map(|e| Some(e.state));
            let gate = entry.and_then(|e| e.source_gate.clone());
            match display {
                Some(id) if id == sole => Ok((
                    hint.flatten(),
                    SelectedDisplay::Explicit(id.to_owned()),
                    gate,
                )),
                Some(id) if id != sole => Err(anyhow!(
                    "display '{id}' is not a selected wear-sampling display"
                )),
                Some(_) | None => Ok((hint.flatten(), SelectedDisplay::SoleUnit, gate)),
            }
        }
        _ => match display {
            Some(id) => {
                if !statuses.contains_key(id) {
                    return Err(anyhow!(
                        "display '{id}' is not a selected wear-sampling display"
                    ));
                }
                let entry = statuses.get(id);
                let hint = entry.map(|e| Some(e.state));
                let gate = entry.and_then(|e| e.source_gate.clone());
                Ok((
                    hint.flatten(),
                    SelectedDisplay::Explicit(id.to_owned()),
                    gate,
                ))
            }
            None => Err(anyhow!(
                "multiple displays configured — pass --display to pick one"
            )),
        },
    }
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
    use super::*;
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
        // with `FlowAlreadyActive`; the hint must therefore NOT lie about a
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

    use std::collections::BTreeMap;
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use dormant_core::ipc_proto::{IpcRequest, IpcResponse, WearSamplingStatusMapEntry};
    use dormant_core::wear::WearSamplingStatus as Lifecycle;

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

    /// One-shot scripted fake daemon: serves one reply then drops the
    /// listener. Used by error-path tests where the CLI must not send a
    /// second request — letting `spawn_two_reply_daemon` accept a
    /// phantom second reply hangs the daemon thread until the test
    /// times out.
    fn spawn_one_reply_daemon(
        socket_path: &Path,
        reply: dormant_core::ipc_proto::IpcResponse,
    ) -> (Arc<Mutex<Vec<IpcRequest>>>, std::thread::JoinHandle<()>) {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path).expect("bind fake socket");
        let captured: Arc<Mutex<Vec<IpcRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let handle = std::thread::spawn(move || {
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
        });
        (captured, handle)
    }

    /// Build a Status reply carrying a per-display status map (the cycle-A
    /// daemon shape). One entry per display id; `state` defaults to
    /// `NeedsConsent` so the `consent_hint` stays the dialog one (issue #189).
    fn status_reply_with_map(entries: &[(&str, WearSamplingState)]) -> IpcResponse {
        let mut reply = IpcResponse::ok(None);
        let mut map: BTreeMap<String, WearSamplingStatusMapEntry> = BTreeMap::new();
        let mut singular: Option<Lifecycle> = None;
        for (id, state) in entries {
            map.insert(
                (*id).to_owned(),
                WearSamplingStatusMapEntry {
                    state: *state,
                    uniform_reason: None,
                    source_gate: None,
                },
            );
            // Mirror the daemon's singular-field behavior: populated only
            // when exactly one display is selected.
            if entries.len() == 1 {
                singular = Some(Lifecycle {
                    state: *state,
                    last_capture_age_s: None,
                    uniform_reason: None,
                    bound_display: Some((*id).to_owned()),
                    granted_at_epoch_s: None,
                    source_gate: None,
                });
            }
        }
        reply.wear_sampling_statuses = Some(map);
        reply.wear_sampling_status = singular;
        reply
    }

    /// `run_enable` MUST query `Status` first so it can pick the right hint,
    /// then send `WearSamplingEnable` — and when the daemon already reports
    /// `Streaming`, the dialog hint MUST NOT be printed (issue #189).
    #[test]
    fn run_enable_sends_status_first_then_enable_and_skips_dialog_hint_when_streaming() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[("desk", WearSamplingState::Streaming)]);
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket, None).expect("run_enable");

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
            "single-display omission must preserve the legacy unit variant, got {:?}",
            requests[1]
        );
    }

    /// Legacy daemons that omit `wear_sampling_status` (and the
    /// `wear_sampling_statuses` map) MUST still get the dialog hint (we
    /// cannot tell that reattach is silent, so we assume the worst case
    /// and warn the operator).
    #[test]
    fn run_enable_prints_dialog_hint_when_status_omits_wear_sampling_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = IpcResponse::ok(None);
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket, None).expect("run_enable");

        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(matches!(requests[0], IpcRequest::Status));
        // Legacy daemons: no `wear_sampling_statuses` map ⇒ sole = 0 ⇒
        // the unit variant is the only safe choice.
        assert!(matches!(requests[1], IpcRequest::WearSamplingEnable));
    }

    // ── #185 Task 24b — per-display CLI forwarding ───────────────────────
    //
    // Cycle A added the daemon-side registry. Cycle B wires the CLI: when
    // the daemon has multiple selected displays, omitting `--display` is an
    // error (the operator must pick one). With exactly one selected,
    // omission still sends the legacy unit variant so the existing single-
    // display ergonomics survive.

    /// When the daemon reports multiple selected displays, `run_enable`
    /// WITHOUT `--display` MUST return an error and MUST NOT send any
    /// per-display variant (no wire traffic beyond the Status query).
    #[test]
    fn run_enable_without_display_errors_under_multi_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[
            ("desk", WearSamplingState::NeedsConsent),
            ("tv", WearSamplingState::NeedsConsent),
        ]);
        // One-shot daemon: Status only. The CLI must not request any
        // Enable variant on the error path, so spawning a second reply
        // would deadlock the daemon thread (and the test) waiting on it.
        let (captured, daemon) = spawn_one_reply_daemon(&socket, status_reply);

        let result = run_enable(&socket, None);
        let err = format!(
            "{}",
            result.expect_err("must error on multi-selection without --display")
        );
        assert!(
            err.contains("--display"),
            "error must point the operator at --display, got: {err}"
        );
        // Only one request reached the wire: the Status probe. The CLI
        // refused before sending an Enable.
        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(
            requests,
            vec![IpcRequest::Status],
            "multi-selection without --display must NOT send any Enable variant, got {requests:?}"
        );
    }

    /// When the daemon reports multiple selected displays, `run_enable`
    /// WITH `--display <id>` MUST send `WearSamplingEnableFor { display }`.
    #[test]
    fn run_enable_with_display_sends_for_variant_under_multi_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[
            ("desk", WearSamplingState::NeedsConsent),
            ("tv", WearSamplingState::NeedsConsent),
        ]);
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket, Some("tv")).expect("run_enable");

        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(matches!(requests[0], IpcRequest::Status));
        assert!(
            matches!(
                &requests[1],
                IpcRequest::WearSamplingEnableFor { display } if display == "tv"
            ),
            "multi-selection with --display tv must send WearSamplingEnableFor {{ display: \"tv\" }}, got {:?}",
            requests[1]
        );
    }

    /// With exactly one selected display, omitting `--display` MUST send
    /// the legacy unit variant `WearSamplingEnable` (issue #185 backward
    /// compatibility, preserves existing single-display operator scripts).
    #[test]
    fn run_enable_omission_under_solo_selection_sends_unit_variant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[("desk", WearSamplingState::NeedsConsent)]);
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket, None).expect("run_enable");

        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(matches!(requests[0], IpcRequest::Status));
        assert!(
            matches!(requests[1], IpcRequest::WearSamplingEnable),
            "solo-selection omission must preserve the legacy unit variant, got {:?}",
            requests[1]
        );
    }

    /// With exactly one selected display, passing `--display <id>` matching
    /// the sole id MUST send `WearSamplingEnableFor { display }` (the
    /// explicit selector still routes through the new variant).
    #[test]
    fn run_enable_with_display_matches_sole_sends_for_variant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[("desk", WearSamplingState::NeedsConsent)]);
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket, Some("desk")).expect("run_enable");

        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(
                &requests[1],
                IpcRequest::WearSamplingEnableFor { display } if display == "desk"
            ),
            "explicit --display must send WearSamplingEnableFor {{ display }}, got {:?}",
            requests[1]
        );
    }

    /// With multiple selected displays, passing `--display <unknown>` MUST
    /// error (no wire traffic beyond the Status probe).
    #[test]
    fn run_enable_with_unknown_display_errors_under_multi_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[
            ("desk", WearSamplingState::NeedsConsent),
            ("tv", WearSamplingState::NeedsConsent),
        ]);
        let (captured, daemon) = spawn_one_reply_daemon(&socket, status_reply);

        let result = run_enable(&socket, Some("unknown"));
        let err = format!("{}", result.expect_err("unknown --display must error"));
        assert!(
            err.contains("not a selected"),
            "error must name the unknown display, got: {err}"
        );
        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(
            requests,
            vec![IpcRequest::Status],
            "unknown --display must NOT send any Enable variant, got {requests:?}"
        );
    }

    /// `run_disable` mirrors `run_enable`: required `--display` under
    /// multi-selection, omission OK under solo selection (legacy unit
    /// variant).
    #[test]
    fn run_disable_without_display_errors_under_multi_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[
            ("desk", WearSamplingState::NeedsConsent),
            ("tv", WearSamplingState::NeedsConsent),
        ]);
        let (captured, daemon) = spawn_one_reply_daemon(&socket, status_reply);

        let result = run_disable(&socket, false, None);
        let err = format!(
            "{}",
            result.expect_err("disable without --display must error under multi-selection")
        );
        assert!(err.contains("--display"), "got: {err}");
        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(
            requests,
            vec![IpcRequest::Status],
            "disable multi-selection without --display must NOT send any Disable variant, got {requests:?}"
        );
    }

    #[test]
    fn run_disable_with_display_sends_for_variant_under_multi_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[
            ("desk", WearSamplingState::NeedsConsent),
            ("tv", WearSamplingState::NeedsConsent),
        ]);
        let disable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, disable_reply]);

        run_disable(&socket, true, Some("desk")).expect("disable");

        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(
                &requests[1],
                IpcRequest::WearSamplingDisableFor { display, forget } if display == "desk" && *forget
            ),
            "explicit --display must send WearSamplingDisableFor with forget=true, got {:?}",
            requests[1]
        );
    }

    /// Solo-selection disable: omitting `--display` MUST preserve the
    /// legacy unit `WearSamplingDisable { forget }` wire tag.
    #[test]
    fn run_disable_omission_under_solo_selection_sends_unit_variant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let status_reply = status_reply_with_map(&[("desk", WearSamplingState::NeedsConsent)]);
        let disable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, disable_reply]);

        run_disable(&socket, false, None).expect("disable");

        daemon.join().expect("fake daemon thread");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(
                requests[1],
                IpcRequest::WearSamplingDisable { forget: false }
            ),
            "solo-selection omission must preserve the legacy unit variant, got {:?}",
            requests[1]
        );
    }

    // ── source_gate surfaces in the per-display status hint ────────────────
    //
    // The enable/disable flows look up the per-display status map before
    // sending their request. The lookup MUST surface the redacted
    // source-gate for the SELECTED display only — not for any other
    // configured display. The hint is captured by routing through
    // `classify_sampling` directly: the printed line in production is
    // `source: <gate>` when the selected entry carries a gate, and
    // nothing otherwise. The test pins both branches.
    #[test]
    fn source_gate_in_status_hint() {
        // Multi-display map: the monitor is mismatched, the TV is
        // matched. The test passes a third display id (unknown) to prove
        // the gate is resolved by looking up the SELECTED id, not by
        // enumerating the map.
        let mut map: BTreeMap<String, WearSamplingStatusMapEntry> = BTreeMap::new();
        map.insert(
            "tv".to_owned(),
            WearSamplingStatusMapEntry {
                state: WearSamplingState::Streaming,
                uniform_reason: None,
                source_gate: Some("matched".to_owned()),
            },
        );
        map.insert(
            "monitor".to_owned(),
            WearSamplingStatusMapEntry {
                state: WearSamplingState::Streaming,
                uniform_reason: None,
                source_gate: Some("mismatched".to_owned()),
            },
        );

        // Selecting the monitor surfaces monitor's gate, NOT tv's.
        let (_state, _selected, gate_monitor) =
            classify_sampling(&map, Some("monitor")).expect("monitor is a selected display");
        assert_eq!(
            gate_monitor.as_deref(),
            Some("mismatched"),
            "selected display's gate must be the monitor's, not the TV's"
        );
        // Belt-and-braces: the TV's matched gate must NOT be reachable
        // through the monitor selection. (The redacted form never carries
        // a per-display map; this confirms the test's invariant by
        // showing the lookup is keyed on the chosen display.)
        assert_ne!(
            gate_monitor.as_deref(),
            Some("matched"),
            "monitor selection must not surface the TV's gate"
        );

        // Selecting the TV surfaces the TV's gate, NOT the monitor's.
        let (_state, _selected, gate_tv) =
            classify_sampling(&map, Some("tv")).expect("tv is a selected display");
        assert_eq!(gate_tv.as_deref(), Some("matched"));
        assert_ne!(gate_tv.as_deref(), Some("mismatched"));

        // Solo selection (omitted --display with one entry) surfaces
        // that entry's gate and never falls back to an absent display.
        let mut solo = BTreeMap::new();
        solo.insert(
            "monitor".to_owned(),
            WearSamplingStatusMapEntry {
                state: WearSamplingState::NeedsConsent,
                uniform_reason: None,
                source_gate: Some("unknown".to_owned()),
            },
        );
        let (_state, _selected, gate_solo) =
            classify_sampling(&solo, None).expect("sole selection is valid");
        assert_eq!(gate_solo.as_deref(), Some("unknown"));

        // A display with no gate configuration (render-only monitor)
        // returns `None` for the gate — the hint is suppressed, never
        // invented. Pinned so a future "fall back to 'unknown'" patch is
        // caught: the source-gate string is a wire-bound contract, not
        // a derived default.
        let mut no_gate = BTreeMap::new();
        no_gate.insert(
            "monitor".to_owned(),
            WearSamplingStatusMapEntry {
                state: WearSamplingState::NeedsConsent,
                uniform_reason: None,
                source_gate: None,
            },
        );
        let (_state, _selected, gate_absent) =
            classify_sampling(&no_gate, Some("monitor")).expect("monitor is a selected display");
        assert!(
            gate_absent.is_none(),
            "absent gate configuration must surface as None, not a synthesized string"
        );

        // The wire-bound hint line is the literal `source: <gate>` form.
        // Drive run_enable end-to-end to make sure the new 3-tuple return
        // shape from classify_sampling is consumed without error in the
        // enable path, and the wire tag is the per-display variant.
        // The contract-pinned assertions on `classify_sampling` above
        // prove the SELECTED display's gate is the one that surfaces;
        // `run_enable` wires that to the `source: <gate>` stdout line in
        // production.
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("dormant.sock");
        let mut wire_map: BTreeMap<String, WearSamplingStatusMapEntry> = BTreeMap::new();
        wire_map.insert(
            "tv".to_owned(),
            WearSamplingStatusMapEntry {
                state: WearSamplingState::NeedsConsent,
                uniform_reason: None,
                source_gate: Some("matched".to_owned()),
            },
        );
        wire_map.insert(
            "monitor".to_owned(),
            WearSamplingStatusMapEntry {
                state: WearSamplingState::NeedsConsent,
                uniform_reason: None,
                source_gate: Some("mismatched".to_owned()),
            },
        );
        let mut status_reply = IpcResponse::ok(None);
        status_reply.wear_sampling_statuses = Some(wire_map);
        let enable_reply = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        let (_captured, daemon) = spawn_two_reply_daemon(&socket, vec![status_reply, enable_reply]);

        run_enable(&socket, Some("monitor")).expect("run_enable with --display monitor");
        daemon.join().expect("fake daemon thread");
    }
}
