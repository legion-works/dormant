//! Pure panel-identity classification for connector ownership and wear sampling.

/// Evidence that a connector carries the configured panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputPanel {
    /// The identity contains the configured substring, ignoring case.
    Match,
    /// A positively identified different panel occupies the connector.
    Mismatch {
        /// Complete observed claim identity.
        found: String,
    },
    /// Identity is unavailable; preserve normal wake and sampling behaviour.
    Unknown,
}

/// Classify identity evidence without turning missing hot-plug data into loss.
#[must_use]
pub fn classify(expected: &str, found: Option<&str>) -> OutputPanel {
    match found {
        None | Some("") => OutputPanel::Unknown,
        Some(found) if found.to_lowercase().contains(&expected.to_lowercase()) => {
            OutputPanel::Match
        }
        Some(found) => OutputPanel::Mismatch {
            found: found.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputPanel, classify};

    #[test]
    fn panel_match() {
        assert_eq!(
            classify("AG326UZD", Some("AOC:AG326UZD:serial")),
            OutputPanel::Match
        );
    }

    #[test]
    fn panel_match_ignores_case() {
        assert_eq!(
            classify("ag326uzd", Some("AOC:AG326UZD:serial")),
            OutputPanel::Match
        );
    }

    #[test]
    fn panel_mismatch() {
        assert_eq!(
            classify("AOC", Some("SAM:TV")),
            OutputPanel::Mismatch {
                found: "SAM:TV".into()
            }
        );
    }

    #[test]
    fn missing_panel_is_unknown() {
        assert_eq!(classify("AOC", None), OutputPanel::Unknown);
        assert_eq!(classify("AOC", Some("")), OutputPanel::Unknown);
    }
}
