//! `dormantctl watch` — stream daemon events in real time.

use std::path::Path;

use anyhow::Result;
use dormant_core::rules::DaemonEvent;

use dormantctl::client;

/// Run the `watch` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run(socket_path: &Path, json_output: bool) -> Result<()> {
    let (stream, _shutdown) = client::connect_events(socket_path)?;

    for event_result in stream {
        match event_result {
            Ok(event) => {
                if json_output {
                    println!("{}", serde_json::to_string(&event)?);
                } else {
                    print_event(&event);
                }
            }
            Err(e) => {
                eprintln!("event error: {e}");
            }
        }
    }

    Ok(())
}

/// Print a [`DaemonEvent`] as a human-readable line.
fn print_event(event: &DaemonEvent) {
    println!("{}", fmt_event(event));
}

/// Render a [`DaemonEvent`] as the human-readable line `print_event` prints
/// — split out as a pure `-> String` seam so the formatting (including the
/// `Unknown` arm, W2 review fix) is unit-testable without capturing stdout.
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
        DaemonEvent::ConfigReloaded => "config reloaded".to_string(),
        DaemonEvent::WakeRetry { display, attempt } => {
            format!("display {display}: wake retry #{attempt}")
        }
        DaemonEvent::WearSnapshot {
            display,
            total_on_hours,
            sample_count,
        } => {
            format!(
                "display {display}: wear snapshot ({total_on_hours:.1}h, {sample_count} samples)"
            )
        }
        DaemonEvent::CompensationAdvisory {
            display,
            hours_since_long_dwell,
        } => {
            format!(
                "display {display}: compensation advisory ({hours_since_long_dwell}h since long dwell)"
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
}
