//! macOS display catalog probe — reports stable CoreGraphics display selectors.

#![cfg(target_os = "macos")]

use crate::types::ProbeResult;

fn format_selectors(selectors: &[String]) -> String {
    selectors.join(", ")
}

/// Report the online CoreGraphics display selectors without changing display state.
#[must_use]
pub fn probe_macos_display_catalog() -> ProbeResult {
    match dormant_displays::macos_display_catalog::online_selectors() {
        Ok(selectors) if selectors.is_empty() => ProbeResult::skip(
            "macos-display-catalog",
            "no online displays with stable CoreGraphics UUIDs",
        ),
        Ok(selectors) => ProbeResult::pass("macos-display-catalog", format_selectors(&selectors)),
        Err(error) => ProbeResult::fail("macos-display-catalog", error),
    }
}

#[cfg(test)]
mod tests {
    use super::format_selectors;

    #[test]
    fn macos_display_catalog_probe_reports_builtin_cg_selector() {
        assert_eq!(
            format_selectors(&["cg:a1b2c3d4-e5f6-0000-1111-222233334444".to_string()]),
            "cg:a1b2c3d4-e5f6-0000-1111-222233334444"
        );
    }
}
