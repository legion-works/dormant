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
    match event {
        DaemonEvent::SensorChanged { sensor, state } => {
            println!("sensor {sensor}: {state:?}");
        }
        DaemonEvent::ZoneChanged {
            zone,
            present,
            cause,
        } => {
            let status = if *present { "occupied" } else { "empty" };
            println!("zone {zone}: {status} (triggered by {cause})");
        }
        DaemonEvent::DisplayPhase {
            display,
            phase,
            cause,
        } => {
            println!("display {display}: {phase} ({cause})");
        }
        DaemonEvent::ConfigReloaded => {
            println!("config reloaded");
        }
        DaemonEvent::WakeRetry { display, attempt } => {
            println!("display {display}: wake retry #{attempt}");
        }
        DaemonEvent::WearSnapshot {
            display,
            total_on_hours,
            sample_count,
        } => {
            println!(
                "display {display}: wear snapshot ({total_on_hours:.1}h, {sample_count} samples)"
            );
        }
        DaemonEvent::CompensationAdvisory {
            display,
            hours_since_long_dwell,
        } => {
            println!(
                "display {display}: compensation advisory ({hours_since_long_dwell}h since long dwell)"
            );
        }
        DaemonEvent::Unknown => {
            println!("unknown daemon event");
        }
    }
}
