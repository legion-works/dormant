//! `dormantctl watch` — stream daemon events in real time.

use std::path::Path;

use anyhow::Result;
use dormant_core::rules::DaemonEvent;

use dormantctl::client;

/// Run the `watch` command.
///
/// # Errors
///
/// Propagates connection, stream I/O, and unexpected stream-end errors.
pub fn run(socket_path: &Path, json_output: bool) -> Result<()> {
    let (mut stream, _shutdown) = client::connect_events(socket_path)?;

    loop {
        match stream.next() {
            Some(Ok(event)) => {
                if json_output {
                    println!("{}", serde_json::to_string(&event)?);
                } else {
                    print_event(&event);
                }
            }
            Some(Err(e)) => {
                anyhow::bail!("event stream failed: {e:#}");
            }
            None => anyhow::bail!("event stream ended unexpectedly: daemon closed connection"),
        }
    }
}

/// Print a [`DaemonEvent`] as a human-readable line.
fn print_event(event: &DaemonEvent) {
    println!("{}", fmt_event(event));
}

/// Render a [`DaemonEvent`] as the human-readable line `print_event` prints
/// — split out as a pure `-> String` seam so the formatting (including the
/// `Unknown` arm, W2 review fix) is unit-testable without capturing stdout.
#[allow(clippy::too_many_lines)]
fn fmt_event(event: &DaemonEvent) -> String {
    match event {
        DaemonEvent::SensorChanged { sensor, state } => {
            format!("sensor {sensor}: {state:?}")
        }
        DaemonEvent::ZoneChanged {
            zone,
            present,
            cause,
        } => {
            let status = if *present { "occupied" } else { "empty" };
            format!("zone {zone}: {status} (triggered by {cause})")
        }
        DaemonEvent::DisplayPhase {
            display,
            phase,
            cause,
        } => {
            format!("display {display}: {phase} ({cause})")
        }
        DaemonEvent::PauseChanged {
            display,
            paused,
            rule,
        } => {
            format!(
                "display {display}: {} (rule: {})",
                if *paused { "paused" } else { "resumed" },
                rule.as_ref().map_or("global", |r| r.0.as_str()),
            )
        }
        DaemonEvent::ConfigReloaded => "config reloaded".to_string(),
        DaemonEvent::WakeRetry { display, attempt } => {
            format!("display {display}: wake retry #{attempt}")
        }
        DaemonEvent::WearSnapshot {
            display,
            total_on_hours,
            sample_count,
            ..
        } => {
            format!(
                "display {display}: wear snapshot ({total_on_hours:.1}h, {sample_count} samples)"
            )
        }
        DaemonEvent::WearSamplingStarted => "wear sampling started".to_string(),
        DaemonEvent::WearSamplingDegraded { reason } => {
            format!("wear sampling degraded: {reason}")
        }
        DaemonEvent::WearSamplingSourceGate {
            display,
            state,
            observed,
        } => match observed {
            Some(source) => format!("wear source-gate {state} for {display} (observed {source})"),
            None => format!("wear source-gate {state} for {display}"),
        },
        DaemonEvent::CompensationAdvisory {
            display,
            hours_since_long_dwell,
        } => {
            format!(
                "display {display}: compensation advisory ({hours_since_long_dwell}h since long dwell)"
            )
        }
        DaemonEvent::BlankFailure {
            display,
            controller,
            detail,
        } => {
            format!("display {display}: blank failed via {controller}: {detail}")
        }
        DaemonEvent::BlankRecovered { display } => {
            format!("display {display}: blank recovered")
        }
        DaemonEvent::WakeRecovered { display, attempts } => {
            format!("display {display}: wake recovered after {attempts} attempts")
        }
        DaemonEvent::Ownership {
            display,
            owned,
            cause,
            verified,
            degraded,
            ..
        } => {
            let ver = match verified {
                Some(true) => " ✓",
                Some(false) => " ✗",
                None => "",
            };
            let deg = if *degraded { " degraded" } else { "" };
            format!(
                "display {display}: {cause} → {}{}{}",
                if *owned { "ours" } else { "peer" },
                ver,
                deg,
            )
        }
        DaemonEvent::Subscribed => "event stream subscribed".to_string(),
        DaemonEvent::OperationsChanged {
            exercise_in_flight,
            emergency_wake_in_flight,
        } => {
            // Human-terminal noise — the web UI drives its own operations display.
            // cmd_watch users see a one-liner summary.
            let ex_line = if exercise_in_flight.is_empty() {
                String::new()
            } else {
                format!("  exercises in flight: {}", exercise_in_flight.join(", "))
            };
            let ew_display = if *emergency_wake_in_flight {
                "  [EMERGENCY WAKE IN FLIGHT]".to_string()
            } else {
                String::new()
            };
            format!(
                "operations:{}{}{}",
                if exercise_in_flight.is_empty() && !*emergency_wake_in_flight {
                    " idle"
                } else {
                    ""
                },
                ex_line,
                ew_display
            )
        }
        DaemonEvent::Unknown => "unknown daemon event".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::types::DisplayId;

    // W2 review fix: `cmd_watch.rs` had zero `#[test]`s despite `print_event`
    // being the last hop in the "tolerate `DaemonEvent::Unknown`" chain
    // (core wire -> EventStream -> tray -> `dormantctl watch` -> web SPA).
    // Pin the `Unknown` arm plus the two wear-event variants (new in this
    // branch) through the `fmt_event` seam.

    #[test]
    fn fmt_event_unknown_is_unknown_daemon_event() {
        assert_eq!(fmt_event(&DaemonEvent::Unknown), "unknown daemon event");
    }

    #[test]
    fn fmt_event_wear_snapshot_formats_hours_and_sample_count() {
        let event = DaemonEvent::WearSnapshot {
            display: DisplayId("desk".to_string()),
            total_on_hours: 12.34,
            sample_count: 7,
            wear_attribution_mode: dormant_core::wear::WearAttributionMode::Uniform,
        };
        assert_eq!(
            fmt_event(&event),
            "display desk: wear snapshot (12.3h, 7 samples)"
        );
    }

    #[test]
    fn fmt_event_compensation_advisory_formats_hours_since_long_dwell() {
        let event = DaemonEvent::CompensationAdvisory {
            display: DisplayId("desk".to_string()),
            hours_since_long_dwell: 48,
        };
        assert_eq!(
            fmt_event(&event),
            "display desk: compensation advisory (48h since long dwell)"
        );
    }

    #[test]
    fn fmt_event_blank_failure_formats_controller_and_detail() {
        let event = DaemonEvent::BlankFailure {
            display: DisplayId("desk".to_string()),
            controller: "ddcci".to_string(),
            detail: "E_DISPLAY_IO: bus gone".to_string(),
        };
        assert_eq!(
            fmt_event(&event),
            "display desk: blank failed via ddcci: E_DISPLAY_IO: bus gone"
        );
    }

    #[test]
    fn fmt_event_blank_recovered_formats() {
        let event = DaemonEvent::BlankRecovered {
            display: DisplayId("desk".to_string()),
        };
        assert_eq!(fmt_event(&event), "display desk: blank recovered");
    }

    #[test]
    fn fmt_event_wake_recovered_formats_attempts() {
        let event = DaemonEvent::WakeRecovered {
            display: DisplayId("desk".to_string()),
            attempts: 3,
        };
        assert_eq!(
            fmt_event(&event),
            "display desk: wake recovered after 3 attempts"
        );
    }
}

#[cfg(all(test, unix))]
mod stream_end_tests {
    use super::*;
    use dormant_core::ipc_proto::IpcRequest;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn watch_errors_when_daemon_closes_the_event_stream() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake socket");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut reader = BufReader::new(stream.try_clone().expect("clone socket"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("read request");
            assert_eq!(
                serde_json::from_str::<IpcRequest>(request.trim()).expect("parse request"),
                IpcRequest::Events
            );
            let subscribed = serde_json::to_string(&DaemonEvent::Subscribed).expect("serialize");
            stream
                .write_all(subscribed.as_bytes())
                .and_then(|()| stream.write_all(b"\n"))
                .expect("send subscribed event");
        });

        let error = run(&socket, false).expect_err("closed event stream must be an error");

        server.join().expect("fake daemon must finish");
        assert!(
            format!("{error:#}").contains("event stream ended unexpectedly"),
            "stream termination must explain the failure: {error:#}"
        );
    }
}
