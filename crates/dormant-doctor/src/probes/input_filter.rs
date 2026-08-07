//! Input-filter readiness probe — validates the runtime environment for
//! the `ignore_devices` glob-matching evdev filter.
//!
//! ## Platform coverage
//!
//! The probe is platform-neutral at the call-site interface. Linux
//! enumerates `/dev/input/event*` via `std::fs::read_dir` and probes
//! readability by attempting a read-only open; every other platform
//! returns a `NotSupported` stub so `dormantctl doctor input-filter`
//! reports honestly rather than panicking or silently hiding.

use std::path::PathBuf;

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

    probe_event_nodes(globs, live_enumerator)
}

// ── Node list: (path, openable) — injected by tests via the enumerator ──────

/// Each discovered event node and whether the current process can open
/// it read-only.  The `bool` is the result of `File::open(path).is_ok()`.
type NodeList = Vec<(PathBuf, bool)>;

/// A function that discovers event nodes and probes their readability.
type EventEnumerator = fn() -> NodeList;

/// Core probe logic, separated from I/O so synthetic tests can inject
/// a deterministic node list.
#[cfg(target_os = "linux")]
fn probe_event_nodes(globs: &[String], enumerator: EventEnumerator) -> ProbeResult {
    let nodes = enumerator();
    probe_node_list(globs, &nodes)
}

/// Evaluate raw node list — the leaf shared by live and synthetic paths.
#[cfg(target_os = "linux")]
fn probe_node_list(globs: &[String], nodes: &NodeList) -> ProbeResult {
    let total_nodes = nodes.len();

    if total_nodes == 0 {
        return ProbeResult::fail(
            "input-filter",
            format!(
                "no /dev/input/event* nodes found — input filter with ignore list {globs:?} cannot operate",
            ),
        );
    }

    let readable: Vec<_> = nodes.iter().filter(|(_, ok)| *ok).collect();
    let unreadable: Vec<_> = nodes.iter().filter(|(_, ok)| !*ok).collect();

    if readable.is_empty() {
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
        readable = readable.len(),
    );
    if !unreadable.is_empty() {
        let paths: Vec<&str> = unreadable
            .iter()
            .map(|(path, _)| path.to_str().unwrap_or("<non-utf8>"))
            .collect();
        detail.push_str("; unreadable: ");
        detail.push_str(&paths.join(", "));
    }

    ProbeResult::pass("input-filter", detail)
}

// ── Linux: live enumeration via File::open ──────────────────────────────────

#[cfg(target_os = "linux")]
fn live_enumerator() -> NodeList {
    let mut nodes: NodeList = Vec::new();

    let Ok(dir) = std::fs::read_dir("/dev/input") else {
        return nodes; // unreadable directory → empty list → Fail above
    };

    for entry in dir {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("event") {
            continue;
        }
        let path = entry.path();
        // Readability is determined by the process's effective
        // permissions — attempt a read-only open and immediately close.
        // Mode-bit inspection (0o400, 0o440, etc.) is unreliable because
        // ACLs, group membership, and capabilities can all change the
        // effective permission independently of the visible mode bits.
        let openable = std::fs::File::open(&path).is_ok();
        nodes.push((path, openable));
    }

    nodes
}

// ── macOS stub (CGEventTap / Accessibility check) ──────────────────────────

#[cfg(target_os = "macos")]
fn live_enumerator() -> NodeList {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn probe_event_nodes(globs: &[String], _enumerator: EventEnumerator) -> ProbeResult {
    // On macOS, input filtering uses a listen-only CGEventTap, which
    // requires Accessibility permission (`AXIsProcessTrusted`).
    probe_macos_accessibility(globs)
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn probe_node_list(globs: &[String], _nodes: &NodeList) -> ProbeResult {
    probe_macos_accessibility(globs)
}

/// Check whether Accessibility permission has been granted for macOS
/// CGEventTap filtering.
#[cfg(target_os = "macos")]
fn probe_macos_accessibility(globs: &[String]) -> ProbeResult {
    let trusted = macos_check_accessibility();
    if trusted {
        ProbeResult::pass(
            "input-filter",
            format!(
                "Accessibility permission granted; input filter with ignore list {globs:?} is ready"
            ),
        )
    } else {
        ProbeResult::fail(
            "input-filter",
            format!(
                "Accessibility permission denied — input filter with ignore list {globs:?} \
                 requires Accessibility. Grant it in System Settings → \
                 Privacy & Security → Accessibility, then restart dormantd."
            ),
        )
    }
}

// Thin FFI wrapper over `AXIsProcessTrusted`.
#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    // Returns non-zero iff the calling process holds Accessibility permission.
    fn AXIsProcessTrusted() -> u8;
}

/// Safe wrapper for the accessibility check.
#[cfg(target_os = "macos")]
fn macos_check_accessibility() -> bool {
    // Safety: AXIsProcessTrusted is a simple getter with no failure path.
    unsafe { AXIsProcessTrusted() != 0 }
}

// ── Non-Linux, non-macOS stub ──────────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn live_enumerator() -> NodeList {
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe_event_nodes(globs: &[String], _enumerator: EventEnumerator) -> ProbeResult {
    ProbeResult::not_supported(
        "input-filter",
        format!(
            "evdev enumeration is Linux-only — ignore list {globs:?} is configured but \
             this platform does not support /dev/input/event* probing",
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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn non_linux_non_macos_reports_not_supported() {
        let result = probe_input_filter(Some(&["*jiggler*".to_string()]));
        assert_eq!(result.status, ProbeStatus::NotSupported);
        assert!(
            result.detail.contains("Linux-only"),
            "detail should mention platform limitation: {}",
            result.detail
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_pass_or_fail() {
        let result = probe_input_filter(Some(&["*jiggler*".to_string()]));
        assert!(
            matches!(result.status, ProbeStatus::Pass | ProbeStatus::Fail),
            "macOS input-filter probe must report Pass or Fail based on \
             Accessibility permission, got {:?} with detail: {}",
            result.status,
            result.detail,
        );
    }

    // ── Synthetic tests (platform-independent — probe node list directly) ──

    /// Prove the mandated Fail path: ignore list configured, but none of
    /// the discovered event nodes are readable by the process.  The
    /// detail must include the exact permission count (`0 of N`).
    ///
    /// Linux-only: calls `probe_node_list` directly, which on non-Linux
    /// resolves to the `NotSupported` stub and cannot satisfy the
    /// `Pass`/`Fail`/count assertions below.
    #[cfg(target_os = "linux")]
    #[test]
    fn zero_readable_nodes_is_a_fail_with_exact_counts() {
        use crate::types::ProbeStatus;

        let nodes: NodeList = [
            "/dev/input/event0",
            "/dev/input/event1",
            "/dev/input/event3",
        ]
        .into_iter()
        .map(|p| (PathBuf::from(p), false))
        .collect();
        let expected_total = nodes.len();

        let result = probe_node_list(&["*jiggler*".to_string()], &nodes);

        assert_eq!(
            result.status,
            ProbeStatus::Fail,
            "zero readable nodes must be a hard Fail"
        );
        let detail = &result.detail;
        assert!(
            detail.contains(&format!("0 of {expected_total}")),
            "detail must carry exact path/permission count (0 of {expected_total}); got: {detail}",
        );
        assert!(
            detail.contains("Ensure the running user is in the 'input' group."),
            "detail should include input-group hint; got: {detail}",
        );
    }

    /// Prove all-nodes-readable returns Pass with the correct count.
    ///
    /// Linux-only: same reason as `zero_readable_nodes_is_a_fail…`.
    #[cfg(target_os = "linux")]
    #[test]
    fn all_nodes_readable_is_a_pass_with_exact_counts() {
        let nodes: NodeList = ["/dev/input/event0", "/dev/input/event3"]
            .into_iter()
            .map(|p| (PathBuf::from(p), true))
            .collect();
        let expected_total = nodes.len();

        let result = probe_node_list(&["*jiggler*".to_string()], &nodes);

        assert_eq!(
            result.status,
            ProbeStatus::Pass,
            "all readable nodes must Pass"
        );
        assert!(
            result
                .detail
                .contains(&format!("{expected_total} of {expected_total}")),
            "detail must carry exact path/permission count; got: {}",
            result.detail,
        );
    }

    /// Prove mixed readable/unreadable nodes still Pass but list the
    /// unreadable paths in the detail.
    ///
    /// Linux-only: same reason as `zero_readable_nodes_is_a_fail…`.
    #[cfg(target_os = "linux")]
    #[test]
    fn mixed_readable_nodes_pass_with_unreadable_paths_listed() {
        let nodes: NodeList = vec![
            (PathBuf::from("/dev/input/event0"), true),
            (PathBuf::from("/dev/input/event1"), false),
            (PathBuf::from("/dev/input/event3"), true),
        ];

        let result = probe_node_list(&["*jiggler*".to_string()], &nodes);

        assert_eq!(
            result.status,
            ProbeStatus::Pass,
            "at least one readable node must Pass"
        );
        assert!(
            result.detail.contains("2 of 3"),
            "detail must carry exact counts (2 of 3); got: {}",
            result.detail,
        );
        assert!(
            result.detail.contains("event1"),
            "detail must list unreadable path; got: {}",
            result.detail,
        );
    }

    /// Prove the empty-node-list Fail path.
    ///
    /// Linux-only: same reason as `zero_readable_nodes_is_a_fail…`.
    #[cfg(target_os = "linux")]
    #[test]
    fn zero_event_nodes_is_a_fail() {
        let nodes: NodeList = Vec::new();
        let result = probe_node_list(&["*jiggler*".to_string()], &nodes);
        assert_eq!(
            result.status,
            ProbeStatus::Fail,
            "empty node list must Fail"
        );
        assert!(
            result.detail.contains("no /dev/input/event* nodes found"),
            "detail must mention missing nodes; got: {}",
            result.detail,
        );
    }
}
