//! Tracing subscriber setup for the daemon.
//!
//! The filter is taken from `RUST_LOG` when set, otherwise from the config's
//! `daemon.log_level`, which scopes debug and trace verbosity to dormant's own
//! crates while keeping third-party logs at an info floor. `--log-json` selects
//! the structured JSON formatter; otherwise a human-readable formatter is used.
//! Both include the target module and the literal `event = "..."` fields the
//! code emits (grep-stable).

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

/// Crates whose verbosity `daemon.log_level` controls.
const DORMANT_TARGETS: &[&str] = &[
    "dormant_core",
    "dormant_sensors",
    "dormant_displays",
    "dormant_doctor",
    "dormant_render",
    "dormant_web",
    "dormantd",
];

pub(crate) fn filter_directive(level: &str) -> String {
    if level.contains(',') || level.contains('=') {
        return level.to_owned();
    }

    if matches!(level.to_ascii_lowercase().as_str(), "debug" | "trace") {
        let level = level.to_ascii_lowercase();
        return format!(
            "info,{}",
            DORMANT_TARGETS
                .iter()
                .map(|target| format!("{target}={level}"))
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    level.to_owned()
}

/// Initialise the global tracing subscriber.
///
/// `level` is the fallback directive used when `RUST_LOG` is unset (typically
/// `daemon.log_level`). `json` selects the JSON formatter.
///
/// # Errors
///
/// Returns an error if a global subscriber was already installed.
pub fn init(level: &str, json: bool) -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(filter_directive(level)));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);

    if json {
        builder
            .json()
            .try_init()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("install JSON tracing subscriber")
    } else {
        builder
            .try_init()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("install tracing subscriber")
    }
}

#[cfg(test)]
mod tests {
    use super::{DORMANT_TARGETS, filter_directive};
    use tracing_subscriber::EnvFilter;

    #[test]
    fn debug_scopes_dormant_crates_and_keeps_info_floor() {
        let directive = filter_directive("debug");

        assert!(directive.starts_with("info,"));
        assert!(directive.contains("dormantd=debug"));
        assert!(!directive.contains("hyper"));
        assert!(!directive.contains("reqwest"));
        assert!(!directive.contains("rumqttc"));
    }

    #[test]
    fn trace_scopes_dormant_crates_and_keeps_info_floor() {
        let directive = filter_directive("trace");

        assert!(directive.starts_with("info,"));
        assert!(directive.contains("dormantd=trace"));
        assert!(!directive.contains("hyper"));
        assert!(!directive.contains("reqwest"));
        assert!(!directive.contains("rumqttc"));
    }

    #[test]
    fn debug_is_case_insensitive() {
        assert_eq!(filter_directive("DEBUG"), filter_directive("debug"));
    }

    #[test]
    fn quiet_levels_are_unchanged() {
        assert_eq!(filter_directive("info"), "info");
        assert_eq!(filter_directive("warn"), "warn");
    }

    #[test]
    fn full_directives_pass_through_unchanged() {
        assert_eq!(
            filter_directive("info,hyper_util=debug"),
            "info,hyper_util=debug"
        );
        assert_eq!(filter_directive("dormantd=trace"), "dormantd=trace");
    }

    #[test]
    fn every_dormant_target_is_present_in_debug_directive() {
        let directive = filter_directive("debug");

        for target in DORMANT_TARGETS {
            assert!(directive.contains(&format!("{target}=debug")));
        }
    }

    #[test]
    fn dormant_render_is_explicitly_scoped() {
        assert!(filter_directive("debug").contains("dormant_render=debug"));
    }

    #[test]
    fn debug_directive_parses() {
        assert!(EnvFilter::try_new(filter_directive("debug")).is_ok());
    }
}
