//! Read-only health classification for daemon-owned active wear sampling.

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
    {
        let Some(status) = status else {
            return ProbeResult::fail("wear-sampling", "daemon sampler status unavailable");
        };
        if !display_bound {
            return ProbeResult::fail(
                "wear-sampling",
                "configured sampling display is missing from the live display set",
            );
        }

        match status.state {
            WearSamplingState::Streaming => {
                if status.bound_display.as_deref()
                    != config.active_sampling.sampled_display.as_deref()
                {
                    return ProbeResult::fail(
                        "wear-sampling",
                        "consent record display binding does not match configuration",
                    );
                }
                let stale_after = config.sample_interval.saturating_mul(2);
                match status.last_capture_age_s {
                    Some(age) if age <= stale_after.as_secs() => ProbeResult::pass(
                        "wear-sampling",
                        format!("streaming; last capture {age}s ago"),
                    ),
                    Some(age) => ProbeResult::fail(
                        "wear-sampling",
                        format!("streaming but last capture is stale ({age}s ago)"),
                    ),
                    None => ProbeResult::fail(
                        "wear-sampling",
                        "streaming but no capture has been recorded",
                    ),
                }
            }
            WearSamplingState::NeedsConsent => {
                ProbeResult::skip("wear-sampling", "active sampling needs operator consent")
            }
            WearSamplingState::Suspended => ProbeResult::fail(
                "wear-sampling",
                "active sampling suspended: display unavailable",
            ),
            WearSamplingState::ConsentPending
            | WearSamplingState::Connecting
            | WearSamplingState::Cooldown => ProbeResult::skip(
                "wear-sampling",
                format!("active sampling transient ({:?})", status.state),
            ),
            WearSamplingState::Disabled => {
                ProbeResult::fail("wear-sampling", "active sampling disabled by daemon state")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
