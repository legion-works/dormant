//! Platform-independent screensaver settings and scheme allowlist.
//!
//! Split from `crate::screensaver` so the daemon can construct
//! [`ScreensaverSettings`] on any target — only the mpv-backed
//! `MpvPlayer` stays Linux-gated.

use std::time::Duration;

use crate::playlist::PlaylistItem;

/// External deadline for first-frame: a calloop `Timer` is armed by
/// the caller for this duration; if no successful render lands before
/// it fires, the show reply is resolved with `Err(E_RENDER_UNAVAILABLE)`
/// so the engine falls through.
#[cfg(target_os = "linux")]
pub(crate) const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(5);

/// How the screensaver transitions between two consecutive playlist items
/// when the outgoing item ends and the next one becomes decodable.
///
/// The production default is [`TransitionMode::Crossfade`] — measured
/// blend cost is ≈0.9 ms/frame at 3072×1728 for the pure u8 lerp
/// (linearly scales with resolution, never a hot spot).
/// [`TransitionMode::None`] keeps the legacy hard-cut behaviour —
/// useful for benchmarks and environments where the per-frame blend
/// cost isn't worth it.
///
/// The state machine in `crate::linux::state` drives the blend via
/// a calloop timer on the Wayland thread; `None` skips every
/// transition-related field on the [`ScreensaverSettings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionMode {
    /// Per-pixel u8 lerp `out = capture*(1-t) + new*t` driven by a
    /// calloop timer at the configured [`ScreensaverSettings::transition_duration`].
    /// Allocation-free per frame; one `Vec<u8>` capture buffer per
    /// session (~21 MiB at 4K).
    Crossfade,
    /// No transition — successive playlist items cut immediately
    /// (the pre-feature behaviour; preserved byte-identical).
    None,
}

impl TransitionMode {
    /// Parse the TOML value of `ScreensaverConfig::transition` (dormant-core schema)
    /// (after the daemon has extracted it from the config) into a
    /// [`TransitionMode`].
    ///
    /// Note: case-sensitive — `Crossfade` ≠ `crossfade`.  The canonical
    /// lowercase strings are what operators write in the TOML config.
    ///
    /// # Errors
    ///
    /// Returns `Err(msg)` for unknown values; the validation layer
    /// formats the message into an `E_SCREENSAVER_SOURCE`-class
    /// `ValidationError`.
    pub fn from_config_str(s: &str) -> Result<Self, String> {
        match s {
            "crossfade" => Ok(Self::Crossfade),
            "none" => Ok(Self::None),
            other => Err(format!(
                "unknown transition '{other}' (allowed: crossfade, none)"
            )),
        }
    }
}

#[allow(clippy::derivable_impls)] // doc-comment on `default()` explains the user-favoured transitions rationale
impl Default for TransitionMode {
    fn default() -> Self {
        // User asked for transitions — the production default is the
        // crossfade.  Operators who want the legacy hard-cut set
        // `transition = "none"` explicitly.
        Self::Crossfade
    }
}

/// How the screensaver player scales its source video to the rendered output
/// rectangle.
///
/// Maps 1:1 to the four canonical mpv scaling modes; covered end-to-end
/// by the property-readback tests (`mpv_player_sets_scale_mode_properties_*`)
/// and the geometry test (`mpv_player_fill_renders_no_letterbox_on_portrait_fixture`)
/// in this crate — every variant's flags do take effect under
/// `MPV_RENDER_API_TYPE_SW`, so we don't have to scale in our own blit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
    /// Crop-to-fill: source is zoomed so it covers the entire output
    /// rectangle; the off-buffer axis is cropped.  No black bars.
    /// mpv: `panscan=1.0`.
    Fill,
    /// Aspect-fit letterbox: source is scaled to fit inside the output
    /// rectangle while preserving aspect ratio; black bars fill the gap.
    /// mpv: defaults (`keepaspect=yes`, `panscan=0.0`).
    Fit,
    /// Stretch: source is scaled to exactly fill the output rectangle,
    /// distorting aspect ratio.  No black bars, but proportions may look
    /// wrong.  mpv: `keepaspect=no`.
    Stretch,
    /// 1:1 centre: source is shown at native pixel dimensions (no scaling),
    /// centred in the output rectangle.  Black bars fill the gap.  mpv:
    /// `video-unscaled=yes`.
    Center,
}

impl ScaleMode {
    /// Parse the TOML value of `ScreensaverConfig::scale_mode` (dormant-core schema)
    /// (after the daemon has extracted it from the config) into a
    /// [`ScaleMode`].
    ///
    /// Note: case-sensitive — `Fill` ≠ `fill`.  The canonical lowercase
    /// strings are what operators write in the TOML config.
    ///
    /// # Errors
    ///
    /// Returns `Err(msg)` for unknown values; the validation layer
    /// formats the message into an `E_SCREENSAVER_SOURCE`-class
    /// `ValidationError`.
    pub fn from_config_str(s: &str) -> Result<Self, String> {
        match s {
            "fill" => Ok(Self::Fill),
            "fit" => Ok(Self::Fit),
            "stretch" => Ok(Self::Stretch),
            "center" => Ok(Self::Center),
            other => Err(format!(
                "unknown scale_mode '{other}' (allowed: fill, fit, stretch, center)"
            )),
        }
    }
}

#[allow(clippy::derivable_impls)] // doc-comment on `default()` explains the OS-screensaver norm rationale
impl Default for ScaleMode {
    fn default() -> Self {
        // OS-screensaver norm: images fill the monitor (no black bars) —
        // matches user expectation set by GNOME / KDE / Windows
        // screensavers, and fixes the legacy letterbox artefact for
        // non-16:9 source aspect ratios.
        Self::Fill
    }
}

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
#[cfg(any(target_os = "linux", test))]
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
/// The daemon assembles this from a [`dormant_core::config::schema::ScreensaverConfig`]
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
    /// How to scale source frames onto the rendered output rectangle.
    /// See [`ScaleMode`].  Default [`ScaleMode::Fill`].
    pub scale_mode: ScaleMode,
    /// How successive playlist items transition into each other.
    /// See [`TransitionMode`].  Default [`TransitionMode::Crossfade`].
    pub transition: TransitionMode,
    /// Length of the [`TransitionMode::Crossfade`] blend in
    /// [`TransitionMode::None`] this field is present but unused.
    /// Bounded by the validator (100 ms ..= 10 s) — see
    /// `dormant_core::config::validate` screensaver-transition rules.
    pub transition_duration: Duration,
}

impl Default for ScreensaverSettings {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            image_duration: Duration::from_secs(10),
            audio: false,
            scale_mode: ScaleMode::Fill,
            transition: TransitionMode::Crossfade,
            transition_duration: Duration::from_secs(1),
        }
    }
}

/// Per-display micro pixel-shift configuration (OLED-health T10) —
/// threaded to the render sink INDEPENDENTLY of [`ScreensaverSettings`]
/// because only the screensaver surface shifts — the black overlay never
/// shifts (U5: a uniform RGB(0,0,0) field is translation-invariant on
/// OLED, subpixels off), whereas `ScreensaverSettings` only ever reaches
/// the sink when the display's ladder contains a `RenderScreensaver`
/// stage.  Mirrors `dormant_core::config::schema::ScreensaverConfig::shift_px`
/// / `.shift_interval` — the daemon assembles this from
/// `displays.<id>.screensaver.shift_px` / `.shift_interval` in
/// [`crate::linux::LayerShellRenderSink::set_shift`]'s caller
/// (`dormantd::app::build_render_sinks`), regardless of whether that
/// display's ladder ever reaches `RenderScreensaver`.
///
/// A display with NO `[displays.<id>.screensaver]` table never gets a
/// `set_shift` call at all — the sink then keeps [`Self::default`]
/// (`shift_px: 0`, fully disabled), which is byte-identical to the
/// pre-T10 render path (no oversized buffer, no `wp_viewport::set_source`,
/// no timer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShiftSettings {
    /// Raster-walk step size, in **buffer pixels** — NOT compositor /
    /// output pixels.  Probe finding 2: a fractional output scale
    /// multiplies the on-screen physical step (e.g. a 2 buffer-px
    /// step measured as ~2.5 physical px at output scale 1.25); the
    /// shift magnitude is approximate by design (wear-evening, not a
    /// precision tool).  `0` disables the shift entirely: the
    /// Wayland glue never oversizes a buffer, never calls
    /// `wp_viewport::set_source`, and never arms the shift timer —
    /// rendering is byte-identical to the pre-T10 code path.
    /// Validated upstream to `0..=8`
    /// (`dormant_core::config::validate`).
    pub shift_px: u8,
    /// Interval between successive raster-walk steps.  Ignored when
    /// `shift_px == 0`.  Validated upstream to `>= 10s`
    /// (`dormant_core::config::validate`).
    pub shift_interval: Duration,
}

impl Default for ShiftSettings {
    fn default() -> Self {
        Self {
            shift_px: 0,
            shift_interval: Duration::from_secs(120),
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
        // scale_mode defaults to Fill — the OS-screensaver norm.
        assert_eq!(s.scale_mode, ScaleMode::Fill);
        // transition defaults to Crossfade — the production default.
        assert_eq!(s.transition, TransitionMode::Crossfade);
        // transition_duration defaults to 1 s — matches the validate.rs default.
        assert_eq!(s.transition_duration, Duration::from_secs(1));
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
            scale_mode: ScaleMode::Center,
            transition: TransitionMode::None,
            transition_duration: Duration::from_millis(500),
        };
        assert_eq!(s.items.len(), 3);
        assert_eq!(s.items[0].uri, "a.mp4");
        assert_eq!(s.items[0].image_duration, Some(Duration::from_secs(2)));
        assert_eq!(s.items[1].uri, "b.png");
        assert_eq!(s.items[2].image_duration, None);
        assert!(s.items[2].uri.starts_with("https://"));
        assert_eq!(s.image_duration, Duration::from_secs(3));
        assert!(s.audio);
        assert_eq!(s.scale_mode, ScaleMode::Center);
        assert_eq!(s.transition, TransitionMode::None);
        assert_eq!(s.transition_duration, Duration::from_millis(500));
    }

    // ── ShiftSettings ─────────────────────────────────────────────────

    #[test]
    fn shift_settings_default_is_fully_disabled() {
        // shift_px = 0 is the SAFETY invariant: a sink that never
        // receives a `set_shift` call (no `[displays.<id>.screensaver]`
        // table) must render byte-identically to pre-T10.
        let s = ShiftSettings::default();
        assert_eq!(s.shift_px, 0);
        assert_eq!(s.shift_interval, Duration::from_secs(120));
    }

    #[test]
    fn shift_settings_carries_configured_values() {
        let s = ShiftSettings {
            shift_px: 4,
            shift_interval: Duration::from_secs(90),
        };
        assert_eq!(s.shift_px, 4);
        assert_eq!(s.shift_interval, Duration::from_secs(90));
    }

    #[test]
    fn shift_settings_is_copy_and_eq() {
        let a = ShiftSettings {
            shift_px: 2,
            shift_interval: Duration::from_secs(120),
        };
        let b = a; // Copy, not move — proves the derive.
        assert_eq!(a, b);
    }

    // ── TransitionMode ────────────────────────────────────────────────

    #[test]
    fn transition_mode_default_is_crossfade() {
        // User asked for transitions — must default to Crossfade, not None.
        assert_eq!(TransitionMode::default(), TransitionMode::Crossfade);
    }

    #[test]
    fn transition_mode_from_config_str_accepts_both_values() {
        assert_eq!(
            TransitionMode::from_config_str("crossfade").unwrap(),
            TransitionMode::Crossfade
        );
        assert_eq!(
            TransitionMode::from_config_str("none").unwrap(),
            TransitionMode::None
        );
    }

    #[test]
    fn transition_mode_from_config_str_rejects_unknown_values() {
        let err = TransitionMode::from_config_str("fade").unwrap_err();
        assert!(err.contains("unknown transition 'fade'"), "{err}");
        assert!(err.contains("crossfade"), "{err}");
        assert!(err.contains("none"), "{err}");
    }

    #[test]
    fn transition_mode_from_config_str_is_case_sensitive() {
        // Canonical strings are lowercase; wrong-cased values are rejected
        // (config validation surfaces the error first, but the parser is
        // authoritative for the fallback path).
        assert!(TransitionMode::from_config_str("Crossfade").is_err());
        assert!(TransitionMode::from_config_str("NONE").is_err());
    }

    // ── ScaleMode ─────────────────────────────────────────────────────────

    #[test]
    fn scale_mode_default_is_fill() {
        // OS-screensaver norm — must default to Fill (no black bars).
        assert_eq!(ScaleMode::default(), ScaleMode::Fill);
    }

    #[test]
    fn scale_mode_from_config_str_accepts_all_four_modes() {
        assert_eq!(ScaleMode::from_config_str("fill").unwrap(), ScaleMode::Fill);
        assert_eq!(ScaleMode::from_config_str("fit").unwrap(), ScaleMode::Fit);
        assert_eq!(
            ScaleMode::from_config_str("stretch").unwrap(),
            ScaleMode::Stretch
        );
        assert_eq!(
            ScaleMode::from_config_str("center").unwrap(),
            ScaleMode::Center
        );
    }

    #[test]
    fn scale_mode_from_config_str_rejects_unknown_values() {
        let err = ScaleMode::from_config_str("zoom").unwrap_err();
        assert!(err.contains("unknown scale_mode 'zoom'"), "{err}");
        assert!(err.contains("fill"), "{err}");
        assert!(err.contains("fit"), "{err}");
        assert!(err.contains("stretch"), "{err}");
        assert!(err.contains("center"), "{err}");
    }

    #[test]
    fn scale_mode_from_config_str_is_case_sensitive() {
        // The canonical strings are lowercase; wrong-cased values are rejected
        // (config validation surfaces the error first, but the parser is
        // authoritative for the fallback path).
        assert!(ScaleMode::from_config_str("Fill").is_err());
        assert!(ScaleMode::from_config_str("STRETCH").is_err());
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
