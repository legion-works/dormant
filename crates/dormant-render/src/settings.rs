//! Platform-independent screensaver settings and scheme allowlist.
//!
//! Split from [`crate::screensaver`] so the daemon can construct
//! [`ScreensaverSettings`] on any target — only the mpv-backed
//! [`MpvPlayer`] stays Linux-gated.

use std::time::Duration;

use crate::playlist::PlaylistItem;

/// External deadline for first-frame: a calloop `Timer` is armed by
/// the caller for this duration; if no successful render lands before
/// it fires, the show reply is resolved with `Err(E_RENDER_UNAVAILABLE)`
/// so the engine falls through.
pub(crate) const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(5);

/// Decide whether a single playlist item is safe to hand to mpv's
/// `loadfile`.  The allowlist is the PRIMARY security control for
/// screensaver media — mpv's `demuxer-lavf-o` whitelist is
/// empirically inert when set via libmpv's property API (the
/// bracket/quoted forms are accepted by mpv but the underlying
/// ffmpeg `protocol_whitelist` is not applied; the comma form that
/// DOES filter is rejected by libmpv with `MPV_ERROR_OPTION_FORMAT`).
///
/// Rules (case-insensitive scheme comparison where applicable):
///
/// - **Absolute path** (starts with `/`) → allow.  These can't carry a
///   scheme prefix and are unambiguous local files.
/// - **`scheme://` URI** → allow only if the scheme is one of
///   `file`, `http`, `https` (case-insensitive).  Everything else
///   (`ftp:`, `data:`, `concat:`, `subfile:`, `unix:`, `sftp:`,
///   `rtmp:`, `tls:`, …) is rejected — mpv would otherwise silently
///   honour them, and several can exfiltrate data or load remote
///   payloads (`data:text/plain;base64,…`, `concat:` chained
///   playlists, etc.).
/// - **Relative path or filename** (no `://` and not absolute) →
///   allow IFF there's no `:` before any `/` — this rejects the
///   `scheme:opaque` form (`av:x`, `concat:foo|bar`,
///   `subfile:/etc/passwd`) that mpv would otherwise accept.  The
///   guard is conservative: `foo/bar:123` (colon AFTER a slash) is
///   allowed because no scheme prefix is plausible there.
///
/// Empty / whitespace-only items are rejected.
pub(crate) fn scheme_allowed(item: &str) -> bool {
    let s = item.trim();
    if s.is_empty() {
        return false;
    }

    // Absolute path — always a local file.
    if s.starts_with('/') {
        return true;
    }

    // URI form: scheme://…
    if let Some(idx) = s.find("://") {
        let scheme = &s[..idx];
        // RFC 3986 forbids `/` in the scheme component; reject any
        // URI where the "scheme" contains a `/` (malformed — the
        // first `://` is inside a path, not the scheme separator).
        if scheme.contains('/') {
            return false;
        }
        let scheme_lower = scheme.to_ascii_lowercase();
        return matches!(scheme_lower.as_str(), "file" | "http" | "https");
    }

    // No `://`.  Reject the `scheme:opaque` form (`av:x`,
    // `concat:foo|bar`, …) — `:` appearing before any `/` is the
    // tell.  Relative filenames like `foo/bar:123` (colon AFTER a
    // slash) are allowed since no scheme prefix is plausible.
    if let Some(colon_pos) = s.find(':')
        && s[..colon_pos].find('/').is_none()
    {
        return false;
    }

    true
}

/// Per-display screensaver configuration carried by the render sink.
///
/// The daemon assembles this from a [`dormant_core::config::ScreensaverConfig`]
/// at sink-build time — one construction per display, at startup and on
/// every config reload.  Playlist scanning is done at assembly time
/// (not per-show) so the wayland thread never touches the filesystem.
#[derive(Debug, Clone)]
pub struct ScreensaverSettings {
    /// Ordered list of [`PlaylistItem`]s, each with an optional
    /// per-item image duration.  mpv loads the first as the active
    /// playlist entry; the rest queue via `append-play`.
    pub items: Vec<PlaylistItem>,
    /// Global default image duration (mpv's `image-display-duration`).
    /// Applied to any item without a per-item duration set.
    pub image_duration: Duration,
    /// Whether audio should be enabled.  False (the default) mutes the
    /// player at init; a future config can flip this on the
    /// runtime-toggleable `mute` property.
    pub audio: bool,
}

impl Default for ScreensaverSettings {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            image_duration: Duration::from_secs(10),
            audio: false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── ScreensaverSettings ────────────────────────────────────────────

    #[test]
    fn screensaver_settings_default_has_ten_second_image_duration() {
        let s = ScreensaverSettings::default();
        assert!(!s.audio);
        assert_eq!(s.image_duration, Duration::from_secs(10));
        assert!(s.items.is_empty());
    }

    #[test]
    fn screensaver_settings_keeps_items_in_order() {
        let s = ScreensaverSettings {
            items: vec![
                PlaylistItem {
                    uri: "a.mp4".into(),
                    image_duration: Some(Duration::from_secs(2)),
                },
                PlaylistItem {
                    uri: "b.png".into(),
                    image_duration: Some(Duration::from_secs(5)),
                },
                PlaylistItem {
                    uri: "https://example/c.jpg".into(),
                    image_duration: None,
                },
            ],
            image_duration: Duration::from_secs(3),
            audio: true,
        };
        assert_eq!(s.items.len(), 3);
        assert_eq!(s.items[0].uri, "a.mp4");
        assert_eq!(s.items[0].image_duration, Some(Duration::from_secs(2)));
        assert_eq!(s.items[1].uri, "b.png");
        assert_eq!(s.items[2].image_duration, None);
        assert!(s.items[2].uri.starts_with("https://"));
        assert_eq!(s.image_duration, Duration::from_secs(3));
        assert!(s.audio);
    }

    // ── scheme_allowed ────────────────────────────────────────────────

    #[test]
    fn scheme_allowed_accepts_absolute_paths() {
        assert!(scheme_allowed("/tmp/foo.mp4"));
        assert!(scheme_allowed("/"));
        assert!(scheme_allowed("/var/media/photos/img.png"));
    }

    #[test]
    fn scheme_allowed_accepts_relative_paths_without_colon_prefix() {
        assert!(scheme_allowed("foo.mp4"));
        assert!(scheme_allowed("relative/path/foo.mp4"));
        assert!(scheme_allowed("path/with:colon-after-slash")); // colon AFTER `/` is fine
        assert!(scheme_allowed("plain.txt"));
    }

    #[test]
    fn scheme_allowed_accepts_whitelisted_uri_schemes_case_insensitive() {
        assert!(scheme_allowed("file:///tmp/foo.mp4"));
        assert!(scheme_allowed("FILE:///tmp/foo.mp4"));
        assert!(scheme_allowed("File:///tmp/foo.mp4"));
        assert!(scheme_allowed("http://example.com/video.mp4"));
        assert!(scheme_allowed("HTTPS://example.com/x"));
        assert!(scheme_allowed("HtTp://example.com/x"));
    }

    #[test]
    fn scheme_allowed_rejects_non_whitelisted_uri_schemes() {
        assert!(!scheme_allowed("ftp://127.0.0.1/x"));
        assert!(!scheme_allowed("data:text/plain,hi"));
        assert!(!scheme_allowed("concat:foo|bar"));
        assert!(!scheme_allowed("subfile:///etc/passwd"));
        assert!(!scheme_allowed("unix:/tmp/socket"));
        assert!(!scheme_allowed("sftp://example.com/x"));
        assert!(!scheme_allowed("rtmp://stream.example.com/live"));
        assert!(!scheme_allowed("tls://server/x"));
    }

    #[test]
    fn scheme_allowed_rejects_scheme_opaque_forms() {
        assert!(!scheme_allowed("scheme:opaque"));
        assert!(!scheme_allowed("foo:bar"));
        assert!(!scheme_allowed("av:x"));
        assert!(!scheme_allowed(":foo")); // empty scheme
        assert!(!scheme_allowed("foo:")); // trailing colon
    }

    #[test]
    fn scheme_allowed_rejects_malformed_uris() {
        assert!(!scheme_allowed("a/b://x")); // `/` in scheme component
        assert!(!scheme_allowed(""));
        assert!(!scheme_allowed("   "));
    }

    #[test]
    fn scheme_allowed_treats_whitespace_trimmed() {
        assert!(scheme_allowed("  file:///tmp/foo.mp4  "));
        assert!(scheme_allowed("  /tmp/foo.mp4  "));
    }
}
