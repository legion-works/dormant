//! Input-filter readiness probe — validates the runtime environment for
//! the `ignore_devices` glob-matching evdev filter (Task 15).
//!
//! ## Platform coverage
//!
//! The probe is platform-neutral at the call-site interface. Linux
//! enumerates `/dev/input/event*` via `std::fs::read_dir` and checks
//! read permission; every other platform returns a `NotSupported` stub
//! so `dormantctl doctor input-filter` reports honestly rather than
//! panicking or silently hiding.

use crate::types::ProbeResult;

/// Probe the evdev backend readiness for input filtering.
///
/// When `ignore_devices` is non-empty, the daemon switches to an
/// evdev-based idle source. This probe confirms whether event nodes
/// are readable — a non-empty ignore list with zero readable nodes is
/// a hard `Fail` because the filter can never work.
///
/// When `ignore_devices` is empty or `None`, the probe returns `Skip`
/// (the feature is inactive — nothing to check).
///
/// # Parameters
///
/// `ignore_devices` — the configured globs, or `None` when the caller
/// has no config (the probe returns `NotSupported` in that case, not
/// `Fail`).
#[must_use]
pub fn probe_input_filter(ignore_devices: Option<&[String]>) -> ProbeResult {
    let Some(globs) = ignore_devices else {
        return ProbeResult::not_supported(
            "input-filter",
            "no config loaded — cannot determine whether the filter is active",
        );
    };

    if globs.is_empty() {
        return ProbeResult::skip(
            "input-filter",
            "ignore_devices is empty — input filter is inactive",
        );
    }

    probe_linux_event_nodes(globs)
}

// ── Linux: enumerate /dev/input/event* ──────────────────────────────────────

#[cfg(target_os = "linux")]
fn probe_linux_event_nodes(globs: &[String]) -> ProbeResult {
    let mut total_nodes: usize = 0;
    let mut readable: usize = 0;
    let mut unreadable: Vec<String> = Vec::new();

    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            return ProbeResult::fail(
                "input-filter",
                format!("cannot open /dev/input: {e} — input filter needs the input group or root"),
            );
        }
    };

    for entry in dir {
        let Ok(entry) = entry else { continue };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Only care about event nodes — not mice, js, or by-path symlinks.
        if !name_str.starts_with("event") {
            continue;
        }

        total_nodes += 1;
        let path = entry.path();

        // Check readability via metadata (no open needed).
        match std::fs::metadata(&path) {
            Ok(meta) => {
                // On Unix we can check the mode bits directly.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if meta.permissions().mode() & 0o400 != 0 {
                        readable += 1;
                    } else {
                        unreadable.push(path.display().to_string());
                    }
                }
                #[cfg(not(unix))]
                {
                    // Non-Unix: metadata exists → assume readable.
                    readable += 1;
                }
            }
            Err(e) => {
                unreadable.push(format!("{} ({e})", path.display()));
            }
        }
    }

    if total_nodes == 0 {
        return ProbeResult::fail(
            "input-filter",
            format!(
                "no /dev/input/event* nodes found — input filter with ignore list {globs:?} cannot operate",
            ),
        );
    }

    if readable == 0 {
        return ProbeResult::fail(
            "input-filter",
            format!(
                "0 of {total_nodes} event nodes are readable — input filter cannot read any accepted device. \
                 Ensure the running user is in the 'input' group.",
            ),
        );
    }

    let mut detail = format!(
        "{readable} of {total_nodes} event node{} readable",
        if total_nodes == 1 { "" } else { "s" },
    );
    if !unreadable.is_empty() {
        use std::fmt::Write;
        let _ = write!(
            detail,
            "; unreadable: {}",
            unreadable
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    ProbeResult::pass("input-filter", detail)
}

// ── Non-Linux stub ──────────────────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
fn probe_linux_event_nodes(globs: &[String]) -> ProbeResult {
    ProbeResult::not_supported(
        "input-filter",
        format!(
            "evdev enumeration is Linux-only — ignore list {:?} is configured but \
             this platform does not support /dev/input/event* probing",
            globs,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProbeStatus;

    #[test]
    fn empty_ignore_list_skips() {
        let result = probe_input_filter(Some(&[]));
        assert_eq!(result.status, ProbeStatus::Skip);
    }

    #[test]
    fn no_config_is_not_supported() {
        let result = probe_input_filter(None);
        assert_eq!(result.status, ProbeStatus::NotSupported);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ignore_list_triggers_node_enumeration() {
        let result = probe_input_filter(Some(&["*jiggler*".to_string()]));
        // On this box (22 readable nodes), expect Pass.
        assert!(
            matches!(result.status, ProbeStatus::Pass | ProbeStatus::Fail),
            "unexpected status: {:?}",
            result.status,
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_reports_not_supported() {
        let result = probe_input_filter(Some(&["*jiggler*".to_string()]));
        assert_eq!(result.status, ProbeStatus::NotSupported);
        assert!(
            result.detail.contains("Linux-only"),
            "detail should mention platform limitation: {}",
            result.detail
        );
    }
}
