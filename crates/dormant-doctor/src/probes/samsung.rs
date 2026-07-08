//! Probe for Samsung Tizen displays — reachability, power state, token presence.

use dormant_core::config::schema::{Config, Credentials};
use dormant_displays::samsung_tizen;

use crate::ProbeResult;
#[cfg(test)]
use crate::ProbeStatus;

/// Probe all Samsung Tizen displays in the config.
///
/// For each display whose controllers include `"samsung-tizen"` and whose
/// `host` is set, emits probes for:
///
/// - TCP reachability on port 8002 (WebSocket)
/// - TCP reachability on port 8001 (REST)
/// - Power state (REST device-info)
/// - Token presence in `credentials.samsung.<host>`
///
/// Returns a single `Skip` if no samsung-tizen displays are configured.
pub async fn probe_samsung(cfg: &Config, creds: &Credentials) -> Vec<ProbeResult> {
    let mut results = Vec::new();

    for (display_id, display_cfg) in &cfg.displays {
        if !display_cfg.controllers.iter().any(|c| c == "samsung-tizen") {
            continue;
        }
        let Some(host) = &display_cfg.host else {
            results.push(ProbeResult::skip(
                format!("samsung {display_id}"),
                "no host configured for samsung-tizen display",
            ));
            continue;
        };

        // TCP 8002 (WebSocket)
        let ws_label = format!("samsung {host} tcp 8002");
        if samsung_tizen::probe_reachable(host, 8002).await {
            results.push(ProbeResult::pass(ws_label, "reachable"));
        } else {
            results.push(ProbeResult::fail(ws_label, "port 8002 (WS) unreachable"));
        }

        // TCP 8001 (REST)
        let rest_label = format!("samsung {host} tcp 8001");
        if samsung_tizen::probe_reachable(host, 8001).await {
            results.push(ProbeResult::pass(rest_label, "reachable"));
        } else {
            results.push(ProbeResult::fail(
                rest_label,
                "port 8001 (REST) unreachable",
            ));
        }

        // Power state
        let power_label = format!("samsung {host} power");
        match samsung_tizen::probe_power_state(host).await {
            Some(state) => results.push(ProbeResult::pass(power_label, state)),
            None => results.push(ProbeResult::skip(
                power_label,
                "unreachable — power state unknown",
            )),
        }

        // Token presence
        let token_label = format!("samsung {host} token");
        if creds.samsung.get(host).is_some() {
            results.push(ProbeResult::pass(token_label, "token present"));
        } else {
            results.push(ProbeResult::fail(
                token_label,
                format!(
                    "no token in credentials.samsung.\"{host}\" — \
                     run `dormantctl pair samsung {host}`"
                ),
            ));
        }
    }

    if results.is_empty() {
        results.push(ProbeResult::skip(
            "samsung",
            "no samsung-tizen displays configured",
        ));
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::Strictness;
    use dormant_core::config::load_config;
    use indexmap::IndexMap;

    fn test_config_with_samsung(host: &str) -> Config {
        let toml = format!(
            r#"
config_version = 1

[displays.livingroom-tv]
controllers = ["samsung-tizen"]
host = "{host}"
blank_mode = "screen_off_audio_on"
"#
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, &toml).unwrap();
        let (cfg, _warnings) = load_config(&path, Strictness::Warn).unwrap();
        cfg
    }

    fn test_creds_with_token(host: &str, token: &str) -> Credentials {
        Credentials {
            samsung: IndexMap::from([(host.into(), token.into())]),
            ..Credentials::default()
        }
    }

    fn test_creds_empty() -> Credentials {
        Credentials::default()
    }

    #[tokio::test]
    async fn probe_samsung_with_token_emits_expected_checks() {
        let cfg = test_config_with_samsung("192.0.2.7");
        let creds = test_creds_with_token("192.0.2.7", "test-token-abc");

        let results = probe_samsung(&cfg, &creds).await;

        assert!(!results.is_empty(), "should produce probe results");

        // Token present check should pass.
        let token_check = results
            .iter()
            .find(|r| r.name.contains("token"))
            .expect("should have a token presence check");
        assert_eq!(token_check.status, ProbeStatus::Pass);

        // TCP checks should exist (they will likely fail against fake host).
        let tcp_checks: Vec<_> = results.iter().filter(|r| r.name.contains("tcp")).collect();
        assert!(
            !tcp_checks.is_empty(),
            "should have at least one TCP reachability check, got {}",
            tcp_checks.len()
        );
    }

    #[tokio::test]
    async fn probe_samsung_missing_token_warns() {
        let cfg = test_config_with_samsung("10.1.1.8");
        let creds = test_creds_empty();

        let results = probe_samsung(&cfg, &creds).await;

        let token_check = results
            .iter()
            .find(|r| r.name.contains("token"))
            .expect("should have a token presence check");
        assert!(
            token_check.status == ProbeStatus::Fail || token_check.status == ProbeStatus::Skip,
            "missing token should be Fail or Skip, got {:?}",
            token_check.status
        );
        assert!(
            token_check
                .detail
                .contains("credentials.samsung.\"10.1.1.8\""),
            "detail should name the creds key; got: {}",
            token_check.detail
        );
    }

    #[tokio::test]
    async fn probe_samsung_no_displays_skips() {
        let mut cfg = test_config_with_samsung("192.0.2.7");
        cfg.displays.clear();
        let creds = test_creds_empty();

        let results = probe_samsung(&cfg, &creds).await;

        assert_eq!(results.len(), 1, "should have exactly one result");
        assert_eq!(results[0].status, ProbeStatus::Skip);
        assert!(
            results[0].detail.contains("no samsung-tizen displays"),
            "skip reason should mention no displays; got: {}",
            results[0].detail
        );
    }
}
