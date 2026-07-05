//! `dormant-doctor` — hardware/connectivity health checks.
//!
//! Probes configured sensors, displays, and credentials to diagnose
//! connectivity and capability issues.  Used by `dormantctl doctor` (offline
//! CLI) and by `dormantd` (online, live-owned-state via `DoctorService`).
//!
//! Wire types (`DoctorReport`, `Check`, `CheckStatus`) are defined in
//! `dormant_core::doctor` so they are reachable from every crate without a
//! cycle.  This crate re-exports them and owns the probe logic.

pub mod probes;
mod types;

pub use dormant_core::doctor::{Check, CheckStatus, DoctorReport};
pub use types::{ProbeResult, ProbeStatus};

use dormant_core::config::Config;
use dormant_core::config::schema::Credentials;

/// Map internal probe results into a `DoctorReport`.
///
/// Pass → `Ok`, Fail → `Fail`, Skip → `Skip`.  Callers add `NotSupported`
/// entries directly when building the report.
#[must_use]
pub fn to_report(results: &[ProbeResult]) -> DoctorReport {
    DoctorReport {
        checks: results
            .iter()
            .map(|r| Check {
                name: r.name.clone(),
                status: match r.status {
                    ProbeStatus::Pass => CheckStatus::Ok,
                    ProbeStatus::Fail => CheckStatus::Fail,
                    ProbeStatus::Skip => CheckStatus::Skip,
                },
                detail: if r.detail.is_empty() {
                    None
                } else {
                    Some(r.detail.clone())
                },
            })
            .collect(),
    }
}

/// Run all applicable offline probes against the given config and credentials.
///
/// This is the "bare `dormantctl doctor`" path: validate config first, then
/// probe every sensor + DDC/CI display in parallel.  Returns a `DoctorReport`
/// suitable for IPC or CLI rendering.
pub async fn probe_all_offline(cfg: &Config, creds: &Credentials) -> DoctorReport {
    let mut results = Vec::new();

    // Config probe first.
    let config_result = probes::config::probe_config_inner(cfg, creds);
    let config_ok = config_result.status != ProbeStatus::Fail;
    results.push(config_result);

    // Collect sensor probes.
    let mut sensor_futs: Vec<std::pin::Pin<Box<dyn futures_util::Future<Output = ProbeResult>>>> =
        Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        if !config_ok {
            // Skip dependent probes when config is invalid.
            let name = match sensor_cfg {
                dormant_core::config::schema::SensorConfig::Mqtt(_) => format!("mqtt {id}"),
                dormant_core::config::schema::SensorConfig::Ha(_) => format!("ha {id}"),
                dormant_core::config::schema::SensorConfig::UsbLd2410(usb_cfg) => {
                    format!("usb {}", usb_cfg.port)
                }
            };
            results.push(ProbeResult::skip(name, "config invalid — fix config first"));
            continue;
        }
        match sensor_cfg {
            dormant_core::config::schema::SensorConfig::Mqtt(mqtt_cfg) => {
                let id = id.clone();
                let cfg = mqtt_cfg.clone();
                sensor_futs.push(Box::pin(async move {
                    probes::mqtt::probe_mqtt_one(&id, &cfg).await
                }));
            }
            dormant_core::config::schema::SensorConfig::Ha(ha_cfg) => {
                let id = id.clone();
                let cfg = ha_cfg.clone();
                let creds = creds.clone();
                sensor_futs.push(Box::pin(async move {
                    probes::ha::probe_ha_one(&id, &cfg, &creds).await
                }));
            }
            dormant_core::config::schema::SensorConfig::UsbLd2410(usb_cfg) => {
                let port = usb_cfg.port.clone();
                let baud = usb_cfg.baud;
                sensor_futs.push(Box::pin(async move {
                    probes::usb::probe_usb(&port, baud).await
                }));
            }
        }
    }

    // Run sensor probes in parallel.
    if !sensor_futs.is_empty() {
        let sensor_results = futures_util::future::join_all(sensor_futs).await;
        results.extend(sensor_results);
    }

    // DDC/CI probe if any display uses ddcci (serial after sensors).
    #[cfg(target_os = "linux")]
    if config_ok {
        let has_ddcci = cfg
            .displays
            .values()
            .any(|d| d.controllers.iter().any(|c| c == "ddcci"));
        if has_ddcci {
            results.push(probes::ddcci::probe_ddcci().await);
        }
    }

    to_report(&results)
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ProbeResult construction ────────────────────────────────────────────

    #[test]
    fn probe_result_pass() {
        let r = ProbeResult::pass("ddcci", "2 displays found");
        assert_eq!(r.name, "ddcci");
        assert_eq!(r.status, ProbeStatus::Pass);
        assert_eq!(r.detail, "2 displays found");
    }

    #[test]
    fn probe_result_fail() {
        let r = ProbeResult::fail("usb /dev/ttyUSB0", "port not found");
        assert_eq!(r.status, ProbeStatus::Fail);
    }

    #[test]
    fn probe_result_skip() {
        let r = ProbeResult::skip("mqtt", "no MQTT sensors");
        assert_eq!(r.status, ProbeStatus::Skip);
    }

    // ── to_report mapper ───────────────────────────────────────────────────

    #[test]
    fn to_report_maps_pass_to_ok() {
        let results = [ProbeResult::pass("a", "good")];
        let report = to_report(&results);
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, CheckStatus::Ok);
        assert_eq!(report.checks[0].detail.as_deref(), Some("good"));
    }

    #[test]
    fn to_report_maps_fail_to_fail() {
        let results = [ProbeResult::fail("a", "bad")];
        let report = to_report(&results);
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
    }

    #[test]
    fn to_report_maps_skip_to_skip() {
        let results = [ProbeResult::skip("a", "n/a")];
        let report = to_report(&results);
        assert_eq!(report.checks[0].status, CheckStatus::Skip);
    }

    #[test]
    fn to_report_empty_detail_becomes_none() {
        let results = [ProbeResult::pass("a", "")];
        let report = to_report(&results);
        assert_eq!(report.checks[0].detail, None);
    }
}
