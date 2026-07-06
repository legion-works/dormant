//! Configuration validation probe.

use crate::types::ProbeResult;
use dormant_core::config::schema::Credentials;
use dormant_core::config::validate;
use dormant_displays::registry::capabilities;

/// Probe the loaded configuration for validation errors.
pub fn probe_config_inner(cfg: &dormant_core::config::Config, creds: &Credentials) -> ProbeResult {
    let errors = validate(cfg, &capabilities(), creds);
    if errors.is_empty() {
        ProbeResult::pass("config", "configuration OK")
    } else {
        let detail: String = errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        ProbeResult::fail("config", detail)
    }
}
