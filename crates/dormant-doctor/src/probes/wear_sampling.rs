//! Read-only health classification for daemon-owned active wear sampling.
//!
//! Cycle A (issue #185) shipped the singular `probe_wear_sampling` for
//! single-display configs.  Cycle B (this commit) adds
//! `probe_wear_sampling_per_display` for multi-display configs — one
//! `ProbeResult` per selected display, never collapsed to a single row,
//! so the operator sees per-display health without expanding a combined
//! entry.  Both probes share the same secret-redaction discipline
//! (`uniform_reason` / `bound_display` are NEVER copied into a detail
//! string; only the lifecycle `state` and a redacted age summary
//! surface).

use std::collections::BTreeMap;

use dormant_core::config::schema::WearConfig;
#[cfg(target_os = "linux")]
use dormant_core::wear::WearSamplingState;
use dormant_core::wear::WearSamplingStatus;

use crate::types::ProbeResult;

/// Classify the daemon-owned, redacted sampler status without opening a source.
#[must_use]
pub fn probe_wear_sampling(
    config: &WearConfig,
    display_bound: bool,
    status: Option<&WearSamplingStatus>,
) -> ProbeResult {
    if !config.active_sampling.enabled {
        return ProbeResult::skip("wear-sampling", "active sampling disabled by configuration");
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (display_bound, status);
        return ProbeResult::not_supported(
            "wear-sampling",
            "active sampling is unavailable on this platform",
        );
    }

    #[cfg(target_os = "linux")]
    classify_one_status("wear-sampling", config, display_bound, status, |result| {
        result
    })
}

/// Per-display redacted active-sampling probe (issue #185 cycle B).
///
/// Returns ONE [`ProbeResult`] PER entry in `configured_display_ids` —
/// never zero, never collapsed to a single row.  The returned order
/// matches the input order so callers can pair each result with its
/// display id without re-sorting.  Each result carries the display id
/// in `subject` and a `"platform"` `category` so the doctor surface
/// can group / filter on it.
///
/// `live_display_ids` is the snapshot's set of currently-present
/// display ids; the probe attributes "missing" results to displays
/// that are configured but absent at runtime, mirroring the singular
/// probe's "configured sampling display is missing from the live
/// display set" failure mode.
///
/// `statuses` is the per-display redacted sampler status map.  When a
/// configured display is absent from the map (e.g. the daemon has not
/// spawned a sampler for it yet), the probe treats it as
/// `NeedsConsent` — the same default the singular probe reports when
/// the daemon status is `None`.
#[must_use]
pub fn probe_wear_sampling_per_display(
    config: &WearConfig,
    configured_display_ids: &[String],
    live_display_ids: &[String],
    statuses: &BTreeMap<String, WearSamplingStatus>,
) -> Vec<ProbeResult> {
    // Disabled-by-config is the single result for every selected
    // display — there is no per-display work to do, but we still emit
    // one row per display so the operator's view of "I have N sampling
    // displays, all disabled" is consistent.
    if !config.active_sampling.enabled {
        return configured_display_ids
            .iter()
            .map(|display| {
                ProbeResult::skip("wear-sampling", "active sampling disabled by configuration")
                    .with_category("platform")
                    .with_subject(display.clone())
            })
            .collect();
    }

    // Off-Linux: one "not supported" row per configured display.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = statuses;
        return configured_display_ids
            .iter()
            .map(|display| {
                ProbeResult::not_supported(
                    "wear-sampling",
                    "active sampling is unavailable on this platform",
                )
                .with_category("platform")
                .with_subject(display.clone())
            })
            .collect();
    }

    #[cfg(target_os = "linux")]
    configured_display_ids
        .iter()
        .map(|display| {
            let status = statuses.get(display);
            let display_bound = live_display_ids.iter().any(|id| id == display);
            let mut result =
                classify_one_status("wear-sampling", config, display_bound, status, |result| {
                    result
                });
            result.subject = Some(display.clone());
            result.category = Some("platform".into());
            result
        })
        .collect()
}

/// Internal helper: classify one display's status, optionally
/// pre-decorating the result with a subject.  The closure exists only
/// to let the singular probe keep the current no-subject shape while
/// the per-display probe injects `subject = Some(display)` post-hoc.
#[cfg(target_os = "linux")]
fn classify_one_status(
    name: &'static str,
    config: &WearConfig,
    display_bound: bool,
    status: Option<&WearSamplingStatus>,
    mut decorate: impl FnMut(ProbeResult) -> ProbeResult,
) -> ProbeResult {
    let Some(status) = status else {
        return decorate(ProbeResult::fail(name, "daemon sampler status unavailable"));
    };
    if !display_bound {
        return decorate(ProbeResult::fail(
            name,
            "configured sampling display is missing from the live display set",
        ));
    }

    let result = match status.state {
        WearSamplingState::Streaming => {
            if status
                .bound_display
                .as_deref()
                .is_some_and(|b| Some(b) != config.active_sampling.first_sampled_display())
            {
                ProbeResult::fail(
                    name,
                    "consent record display binding does not match configuration",
                )
            } else {
                let stale_after = config.sample_interval.saturating_mul(2);
                match status.last_capture_age_s {
                    Some(age) if age <= stale_after.as_secs() => {
                        ProbeResult::pass(name, format!("streaming; last capture {age}s ago"))
                    }
                    Some(age) => ProbeResult::fail(
                        name,
                        format!("streaming but last capture is stale ({age}s ago)"),
                    ),
                    None => ProbeResult::fail(name, "streaming but no capture has been recorded"),
                }
            }
        }
        WearSamplingState::NeedsConsent => {
            ProbeResult::skip(name, "active sampling needs operator consent")
        }
        WearSamplingState::Suspended => {
            ProbeResult::fail(name, "active sampling suspended: display unavailable")
        }
        WearSamplingState::ConsentPending
        | WearSamplingState::Connecting
        | WearSamplingState::Cooldown => ProbeResult::skip(
            name,
            format!("active sampling transient ({:?})", status.state),
        ),
        WearSamplingState::Disabled => {
            ProbeResult::fail(name, "active sampling disabled by daemon state")
        }
    };
    decorate(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProbeStatus;
    #[cfg(target_os = "linux")]
    use dormant_core::wear::WearSamplingState;

    fn config() -> WearConfig {
        WearConfig {
            sample_interval: std::time::Duration::from_secs(30),
            active_sampling: dormant_core::config::schema::ActiveSamplingConfig {
                enabled: true,
                sampled_display: Some("oled".into()),
                ..Default::default()
            },
            ..WearConfig::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn status(state: WearSamplingState, age: Option<u64>) -> WearSamplingStatus {
        WearSamplingStatus {
            state,
            last_capture_age_s: age,
            uniform_reason: None,
            bound_display: Some("oled".into()),
            granted_at_epoch_s: None,
            source_gate: None,
        }
    }

    #[test]
    fn probe_id_is_wear_sampling() {
        let result = probe_wear_sampling(&config(), false, None);
        assert_eq!(result.name, "wear-sampling");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn streaming_fresh_is_healthy() {
        let result = probe_wear_sampling(
            &config(),
            true,
            Some(&status(WearSamplingState::Streaming, Some(2))),
        );
        assert_eq!(result.status, crate::types::ProbeStatus::Pass);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn streaming_stale_is_failed() {
        let result = probe_wear_sampling(
            &config(),
            true,
            Some(&status(WearSamplingState::Streaming, Some(120))),
        );
        assert_eq!(result.status, crate::types::ProbeStatus::Fail);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn needs_consent_is_skipped_without_secret_details() {
        let result = probe_wear_sampling(
            &config(),
            true,
            Some(&status(WearSamplingState::NeedsConsent, None)),
        );
        assert_eq!(result.status, crate::types::ProbeStatus::Skip);
        assert!(!result.detail.contains("token"));
        assert!(!result.detail.contains("persistent"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn invalid_display_binding_fails() {
        let result = probe_wear_sampling(
            &config(),
            false,
            Some(&status(WearSamplingState::Streaming, Some(2))),
        );
        assert_eq!(result.status, crate::types::ProbeStatus::Fail);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn suspended_is_failed() {
        let result = probe_wear_sampling(
            &config(),
            true,
            Some(&status(WearSamplingState::Suspended, None)),
        );
        assert_eq!(result.status, crate::types::ProbeStatus::Fail);
    }

    /// `CaptureSource` and portal commands live in `dormantd`, which depends on
    /// this crate; the dependency graph makes them structurally unreachable
    /// here. The probe contract therefore accepts only config and redacted data.
    #[test]
    fn probe_contract_has_no_capture_capability() {
        let _: fn(&WearConfig, bool, Option<&WearSamplingStatus>) -> ProbeResult =
            probe_wear_sampling;
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn details_are_redacted_from_consent_secrets() {
        let mut sampler = status(WearSamplingState::NeedsConsent, None);
        sampler.uniform_reason = Some("token=persistent-id-should-not-leak".into());
        sampler.bound_display = Some("persistent-id-should-not-leak".into());
        let result = probe_wear_sampling(&config(), true, Some(&sampler));
        assert!(!result.detail.contains("token"));
        assert!(!result.detail.contains("persistent-id"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_is_unavailable() {
        let result = probe_wear_sampling(&config(), true, None);
        assert_eq!(result.status, crate::types::ProbeStatus::NotSupported);
    }

    // ── #185 Task 24b cycle B — per-display probe ────────────────────────

    fn two_display_config() -> WearConfig {
        WearConfig {
            sample_interval: std::time::Duration::from_secs(30),
            active_sampling: dormant_core::config::schema::ActiveSamplingConfig {
                enabled: true,
                sampled_display: None,
                sampled_displays: vec!["desk".into(), "tv".into()],
                ..Default::default()
            },
            ..WearConfig::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn status_for(display: &str, state: WearSamplingState) -> WearSamplingStatus {
        WearSamplingStatus {
            state,
            last_capture_age_s: Some(2),
            uniform_reason: None,
            bound_display: Some(display.to_owned()),
            granted_at_epoch_s: None,
            source_gate: None,
        }
    }

    /// Per-display probe MUST return exactly N results when the
    /// operator has configured N displays — neither zero (silently
    /// drops the operator's view) nor one (collapses the data and
    /// would let a single-display implementation pass).
    #[cfg(target_os = "linux")]
    #[test]
    fn per_display_returns_one_result_per_configured_display() {
        let statuses: BTreeMap<String, WearSamplingStatus> = [
            (
                "desk".to_owned(),
                status_for("desk", WearSamplingState::Streaming),
            ),
            (
                "tv".to_owned(),
                status_for("tv", WearSamplingState::NeedsConsent),
            ),
        ]
        .into_iter()
        .collect();
        let results = probe_wear_sampling_per_display(
            &two_display_config(),
            &["desk".to_owned(), "tv".to_owned()],
            &["desk".to_owned(), "tv".to_owned()],
            &statuses,
        );
        assert_eq!(
            results.len(),
            2,
            "must return one ProbeResult per configured display (2), got {}",
            results.len()
        );
    }

    /// Each result's `subject` MUST carry the display id the probe was
    /// attributed to — a regression that emits two results with the
    /// same subject (or no subject) would still pass the count
    /// assertion above.
    #[cfg(target_os = "linux")]
    #[test]
    fn per_display_each_result_carries_its_own_subject() {
        let statuses: BTreeMap<String, WearSamplingStatus> = [
            (
                "desk".to_owned(),
                status_for("desk", WearSamplingState::Streaming),
            ),
            (
                "tv".to_owned(),
                status_for("tv", WearSamplingState::NeedsConsent),
            ),
        ]
        .into_iter()
        .collect();
        let results = probe_wear_sampling_per_display(
            &two_display_config(),
            &["desk".to_owned(), "tv".to_owned()],
            &["desk".to_owned(), "tv".to_owned()],
            &statuses,
        );
        let subjects: std::collections::BTreeSet<&str> = results
            .iter()
            .filter_map(|r| r.subject.as_deref())
            .collect();
        assert_eq!(
            subjects,
            ["desk", "tv"].into_iter().collect(),
            "every result must carry its own subject (no duplicates, no missing)"
        );
    }

    /// Per-display probe MUST redact consent secrets in EVERY result
    /// detail.  A regression that emits the per-display status's
    /// `uniform_reason` or `bound_display` verbatim would surface
    /// `persistent_id` / portal-token-shaped strings to the doctor
    /// output.
    #[cfg(target_os = "linux")]
    #[test]
    fn per_display_details_redact_consent_secrets() {
        let mut secret_status = status_for("desk", WearSamplingState::NeedsConsent);
        secret_status.uniform_reason = Some("token=persistent-id-should-not-leak".into());
        secret_status.bound_display = Some("persistent-id-should-not-leak".into());
        let statuses: BTreeMap<String, WearSamplingStatus> =
            [("desk".to_owned(), secret_status)].into_iter().collect();
        let results = probe_wear_sampling_per_display(
            &two_display_config(),
            &["desk".to_owned()],
            &["desk".to_owned()],
            &statuses,
        );
        assert_eq!(results.len(), 1);
        let detail = &results[0].detail;
        assert!(!detail.contains("token"), "detail leaked token: {detail}");
        assert!(
            !detail.contains("persistent-id"),
            "detail leaked persistent-id: {detail}"
        );
    }

    /// Disabled-by-config still emits one row per display — keeps the
    /// operator's view consistent.
    #[test]
    fn per_display_disabled_emits_one_per_display() {
        let mut cfg = config();
        cfg.active_sampling.enabled = false;
        let results = probe_wear_sampling_per_display(
            &cfg,
            &["desk".to_owned(), "tv".to_owned()],
            &["desk".to_owned(), "tv".to_owned()],
            &BTreeMap::new(),
        );
        assert_eq!(results.len(), 2, "got {}", results.len());
        assert!(results.iter().all(|r| r.status == ProbeStatus::Skip));
    }
}
