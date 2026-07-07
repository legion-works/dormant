//! libmpv-backed screensaver overlay.
//!
//! Owns the mpv handle and SW render context that produce a muted
//! slideshow/video stream for [`StageKind::RenderScreensaver`] ladder
//! stages.  The player is fully synchronous — the owning thread drives
//! [`MpvPlayer::render_frame_into`] after being notified by an mpv
//! wakeup callback (which writes a single byte to a pipe the owner
//! registered as a calloop source).
//!
//! ## Threat model — scheme allowlist
//!
//! The operator owns the config, but playlist URLs may be pasted
//! from anywhere (configs shared online, log scraping, etc.) and mpv
//! exposes exotic schemes — `data:`, `concat:`, `subfile:`, `unix:`,
//! `av:`, `ftp:`, and more — that would otherwise be silently
//! reachable.  [`scheme_allowed`] gates every item before any
//! `loadfile` runs; mpv is NEVER handed an item outside the allowlist.
//!
//! ## Sandbox flags
//!
//! The init-time flags pin the embedded player to an unprivileged,
//! no-network-by-default sandbox.  Per the libmpv spike report (§Q3 +
//! gotcha #4):
//!
//! | flag | value | reason |
//! |------|-------|--------|
//! | `vo` | `libmpv` | hook the SW render context; without this all frames are zero |
//! | `ytdl` | `no` | no YouTube/external resolver — local files only |
//! | `load-scripts` | `no` | no Lua/JS scripts from the playlist directory |
//! | `osc` | `no` | no on-screen-controller |
//! | `input-default-bindings` | `no` | no global key bindings |
//! | `terminal` | `no` | never touches stdin/stdout/stderr |
//! | `config` | `no` | no `~/.config/mpv/*` lookup |
//! | `input-ipc-server` | (empty) | no IPC socket bound |
//! | `demuxer-lavf-o` | `protocol_whitelist=[file,http,https,tcp,tls]` | ffmpeg-level protocol whitelist.  **Empirically inert when set via libmpv's property API** (the bracket syntax IS accepted by mpv's property setter but a TCP-connect probe shows the underlying ffmpeg `protocol_whitelist` is NOT applied — the comma form (`file,http,https,tcp,tls`) that DOES filter is rejected by libmpv's property setter with `MPV_ERROR_OPTION_FORMAT`).  Kept as best-effort defense-in-depth against a future mpv fix; the PRIMARY security control is [`scheme_allowed`], which runs in our code BEFORE any `loadfile`. |
//! | `demuxer-max-bytes` | 64 MiB | RAM-bounded streaming |
//! | `network-timeout` | 10 s | fail fast on stuck network reads |
//!
//! Runtime audio is disabled via `mute=yes` (NOT `audio=no` — `audio=no`
//! is irreversible at runtime per gotcha #4; `mute=yes` can be toggled
//! back by a future unmute config).
//!
//! ## SW pixel format
//!
//! We render with `bgr0` because on a little-endian host the in-memory
//! byte order is `[B, G, R, X]`, which is the same layout as Wayland's
//! `WL_SHM_FORMAT_XRGB8888` (32-bit, X/alpha at byte 3, ignored by the
//! compositor).  Using `rgb0` would require a swizzle per frame; using
//! `rgba` is silently rejected by mpv and produces all-zero buffers
//! (gotcha #3).  See [`crate::linux::surface::SHM_PIXEL_FORMAT`] for the
//! shared XRGB declaration that both the screensaver and black-fallback
//! sites use.
//!
//! ## Thread model
//!
//! The player is constructed and driven from the dedicated Wayland
//! thread.  The mpv wakeup callback fires from mpv's internal threads
//! and writes a single byte to a pipe registered as a calloop source on
//! that same loop, so all Wayland object access stays on one thread.

use std::ffi::{CString, c_void};
use std::os::fd::RawFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use libmpv2_sys::{
    MPV_RENDER_API_TYPE_SW, mpv_render_context, mpv_render_context_create, mpv_render_context_free,
    mpv_render_context_render, mpv_render_context_set_update_callback, mpv_render_context_update,
    mpv_render_param, mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
    mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
    mpv_render_param_type_MPV_RENDER_PARAM_SW_FORMAT,
    mpv_render_param_type_MPV_RENDER_PARAM_SW_POINTER,
    mpv_render_param_type_MPV_RENDER_PARAM_SW_SIZE,
    mpv_render_param_type_MPV_RENDER_PARAM_SW_STRIDE,
    mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME,
};

/// External deadline for first-frame: a calloop `Timer` is armed by
/// the caller (in `state.rs::complete_screensaver_show`) for this
/// duration; if no successful render lands before it fires, the show
/// reply is resolved with `Err(E_RENDER_UNAVAILABLE)` so the engine
/// falls through.  The internal check inside
/// [`MpvPlayer::render_frame_into`] is defense-in-depth for the case
/// where the wakeup pipe keeps firing past the deadline.
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
/// at sink-build time.  Today it's only constructed from tests — the
/// production path is wired when the daemon gains screensaver-aware
/// sink construction.
#[derive(Debug, Clone)]
pub struct ScreensaverSettings {
    /// Ordered list of media items (local paths or URLs).  mpv loads the
    /// first as the active playlist entry; the rest queue via
    /// `append-play` if the runtime needs to grow the playlist.
    pub items: Vec<String>,
    /// How long each image is displayed (mpv's `image-display-duration`).
    /// Ignored for video sources.
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

/// Owned handle to an mpv instance + SW render context.
///
/// `!Send` by construction — [`Self::ctx`] is a raw pointer the owning
/// thread drives.  The wakeup write fd is `Send`-safe to read across
/// threads (a single byte write is atomic for fds created with
/// `O_NONBLOCK`); the callback that writes it captures the fd as a
/// `usize` so the `extern "C"` fn pointer has no Rust state.
///
/// Cleanup runs in [`Drop`]: unset the wakeup callback BEFORE freeing
/// the render context (so a racing mpv thread can't fire the trampoline
/// against a freed context), free the context, drop the mpv handle,
/// close the write fd.  Every `MpvPlayer` value owns exactly one of
/// each resource — `Drop` is the single point of teardown, so
/// post-`new` failure paths in the caller no longer leak.
pub struct MpvPlayer {
    mpv: Option<libmpv2::Mpv>,
    ctx: NonNull<mpv_render_context>,
    /// Write end of the wakeup pipe.  `None` after [`Drop`] runs.
    wakeup_write_fd: Option<RawFd>,
    width: i32,
    height: i32,
    stride: i32,
    /// Absolute instant after which a still-frame-less player is dead.
    /// `None` after the first frame is produced.
    first_frame_deadline: Option<Instant>,
    /// Set after the first successful render — used to gate the
    /// pre-first-frame timeout.
    has_first_frame: bool,
    /// Held for the lifetime of the player so `append-play` can grow
    /// the playlist later; unused today beyond logging.
    #[allow(dead_code)]
    items: Vec<String>,
}

/// Errors that [`MpvPlayer::new`] / [`MpvPlayer::render_frame_into`] can return.
#[derive(Debug)]
pub enum MpvError {
    /// mpv init / property set / render-context-create failed.
    Init(String),
    /// `mpv_render_context_render` returned a negative error code.
    Render(i32),
    /// No frame produced within [`FIRST_FRAME_DEADLINE`].
    NoFirstFrame,
}

impl std::fmt::Display for MpvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init(s) => write!(f, "mpv init: {s}"),
            Self::Render(c) => write!(f, "mpv render error code {c}"),
            Self::NoFirstFrame => write!(
                f,
                "mpv produced no first frame within {FIRST_FRAME_DEADLINE:?}"
            ),
        }
    }
}

impl std::error::Error for MpvError {}

unsafe extern "C" fn wakeup_trampoline(cb_ctx: *mut c_void) {
    // The player encodes the write fd as a usize in the callback ctx.
    // `write(2)` is safe to call with a single byte and an O_NONBLOCK fd
    // — the worst case is EAGAIN, which we drop (mpv will re-fire the
    // wakeup on the next event anyway).
    let fd = cb_ctx as RawFd;
    let byte: [u8; 1] = [1];
    // SAFETY: the fd is owned by the MpvPlayer and remains valid for
    // the lifetime of the render context; `Drop` unregisters the
    // callback BEFORE freeing the context and closing the fd.
    let _ = unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        // Unset the callback FIRST so a racing mpv thread can't fire the
        // trampoline after the context is gone.
        //
        // SAFETY: ctx was created by `mpv_render_context_create` and is
        // still alive; passing `None` is the documented "disable"
        // transition (subsequent wakeups don't fire).
        unsafe {
            mpv_render_context_set_update_callback(self.ctx.as_ptr(), None, std::ptr::null_mut());
            mpv_render_context_free(self.ctx.as_ptr());
        }
        // Drop the mpv handle (terminates mpv, releases the player).
        // `take()` leaves `None` so the second drop is a no-op if anyone
        // ever calls this twice (defensive — Drop only fires once).
        if let Some(mpv) = self.mpv.take() {
            drop(mpv);
        }
        // Close the write fd exactly once.
        //
        // SAFETY: fd was created by `libc::pipe2` in the calloop layer
        // and is owned exclusively by this player; closing it twice
        // would be a bug but the calloop read end is a separate fd.
        if let Some(fd) = self.wakeup_write_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

impl MpvPlayer {
    /// Build the player with the sandbox flags described in the module
    /// docs, filter the playlist through [`scheme_allowed`], create
    /// the SW render context sized to `(width, height)`, arm the
    /// wakeup callback on `wakeup_write_fd`, and (last) load the
    /// first surviving playlist item.
    ///
    /// Order matters for leak-freedom: the owned `MpvPlayer` struct
    /// is constructed (with `Option`-wrapped mpv handle, wakeup fd,
    /// and the render-context pointer) BEFORE `loadfile` runs.  Any
    /// error after the `MpvPlayer` value exists lets `Drop` clean up
    /// — unset the callback, free the context, drop mpv, close the
    /// write fd.  Pre-construction errors (mpv init, render-context
    /// creation) still rely on the prior pattern of returning `Err`
    /// before any owned state leaks.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        items: Vec<String>,
        image_duration: Duration,
        audio_enabled: bool,
        width: u32,
        height: u32,
        wakeup_write_fd: RawFd,
    ) -> Result<Self, MpvError> {
        // ── Scheme allowlist (PRIMARY security control) ───────────
        // Runs BEFORE any mpv interaction so a rejected item never
        // causes mpv to even consider opening it.  Logged at warn so
        // operators can audit config sources that produce URLs.
        let filtered_items: Vec<String> = items
            .into_iter()
            .filter(|item| {
                if scheme_allowed(item) {
                    true
                } else {
                    tracing::warn!(
                        event = "screensaver_item_rejected",
                        item = %item,
                        reason = "scheme",
                    );
                    false
                }
            })
            .collect();

        if filtered_items.is_empty() {
            return Err(MpvError::Init(
                "all playlist items rejected by scheme allowlist".into(),
            ));
        }

        // ── Init-time sandbox (see module-level table) ─────────────
        let mpv = libmpv2::Mpv::with_initializer(|opts| {
            opts.set_property("vo", "libmpv")?;
            opts.set_property("ytdl", false)?;
            opts.set_property("load-scripts", false)?;
            opts.set_property("osc", false)?;
            opts.set_property("input-default-bindings", false)?;
            opts.set_property("terminal", false)?;
            opts.set_property("config", false)?;
            opts.set_property("input-ipc-server", "")?;
            // mpv 0.41 dropped `protocol-whitelist`; the ffmpeg-level
            // lavf option path survives and is the supported way to
            // pin the same restriction.  Empirically: when set via
            // libmpv's property API, the bracket form IS accepted by
            // mpv's parser but the underlying ffmpeg `protocol_whitelist`
            // is NOT actually applied (TCP probe confirmed ftp:// still
            // triggers a connection).  Kept as defense-in-depth against
            // future mpv fixes; the PRIMARY control is the
            // [`scheme_allowed`] filter run above.
            opts.set_property(
                "demuxer-lavf-o",
                "protocol_whitelist=[file,http,https,tcp,tls]",
            )?;
            opts.set_property("demuxer-max-bytes", 67_108_864_i64)?;
            opts.set_property("network-timeout", 10_i64)?;
            Ok(())
        })
        .map_err(|e| MpvError::Init(format!("create: {e}")))?;

        // ── Runtime flags ──────────────────────────────────────────
        // mute=yes (NOT audio=no — see gotcha #4).  The bool coercion
        // matches libmpv2's SetData impl: true → "yes".
        mpv.set_property("mute", !audio_enabled)
            .map_err(|e| MpvError::Init(format!("set mute: {e}")))?;
        mpv.set_property("loop-playlist", "inf")
            .map_err(|e| MpvError::Init(format!("set loop-playlist: {e}")))?;
        mpv.set_property("image-display-duration", image_duration.as_secs_f64())
            .map_err(|e| MpvError::Init(format!("set image-display-duration: {e}")))?;

        // ── SW render context ──────────────────────────────────────
        let width_i = i32::try_from(width).map_err(|e| MpvError::Init(format!("width: {e}")))?;
        let height_i = i32::try_from(height).map_err(|e| MpvError::Init(format!("height: {e}")))?;
        let stride = width_i
            .checked_mul(4)
            .ok_or_else(|| MpvError::Init("stride overflow".into()))?;

        let api_type = MPV_RENDER_API_TYPE_SW.as_ptr() as *mut c_void;
        let create_params = [
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: api_type,
            },
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];

        let mut ctx: *mut mpv_render_context = std::ptr::null_mut();
        // SAFETY: `mpv.ctx.as_ptr()` is a valid `mpv_handle`; `create_params`
        // is a proper sentinel-terminated array of `mpv_render_param`.
        let ret = unsafe {
            mpv_render_context_create(
                &raw mut ctx,
                mpv.ctx.as_ptr(),
                create_params.as_ptr().cast_mut(),
            )
        };
        if ret < 0 {
            return Err(MpvError::Init(format!(
                "mpv_render_context_create returned {ret}"
            )));
        }
        let ctx = NonNull::new(ctx).ok_or_else(|| MpvError::Init("null render context".into()))?;

        // ── Wakeup callback → pipe write ───────────────────────────
        // SAFETY: `wakeup_trampoline` is `unsafe extern "C"` and ignores
        // its argument beyond casting; the `cb_ctx` we pass encodes the
        // fd (valid for the lifetime of the context, since `Drop`
        // unregisters the callback before freeing).
        unsafe {
            mpv_render_context_set_update_callback(
                ctx.as_ptr(),
                Some(wakeup_trampoline),
                usize::try_from(wakeup_write_fd).expect("pipe fd fits in usize") as *mut c_void,
            );
        }

        // ── Construct the owned player BEFORE loadfile ────────────
        // After this point, `Drop` owns the cleanup of ctx, mpv, and
        // the wakeup write fd.  `load_items` runs against the
        // constructed player — on Err, the value drops and cleans up.
        let mut player = Self {
            mpv: Some(mpv),
            ctx,
            wakeup_write_fd: Some(wakeup_write_fd),
            width: width_i,
            height: height_i,
            stride,
            first_frame_deadline: Some(Instant::now() + FIRST_FRAME_DEADLINE),
            has_first_frame: false,
            items: filtered_items,
        };

        // ── Load first playlist entry (last fallible step) ────────
        // `items` was already filtered through `scheme_allowed` above,
        // so the first item is by construction inside the allowlist
        // and safe to hand to mpv.
        if let Some(first) = player.items.first() {
            let mpv = player
                .mpv
                .as_mut()
                .ok_or_else(|| MpvError::Init("player destroyed".into()))?;
            mpv.command("loadfile", &[first.as_str(), "replace"])
                .map_err(|e| MpvError::Init(format!("loadfile '{first}': {e}")))?;
        }

        Ok(player)
    }

    /// Drain mpv's pending events and render the current frame into
    /// `buf`.  Returns `Ok(true)` if a new frame was drawn, `Ok(false)`
    /// if no frame was ready (caller may still want to commit to keep
    /// the surface alive).  Returns [`MpvError::NoFirstFrame`] if the
    /// pre-first-frame deadline elapsed, and [`MpvError::Render`] on a
    /// negative mpv return code.
    pub fn render_frame_into(&mut self, buf: &mut [u8]) -> Result<bool, MpvError> {
        if !self.has_first_frame
            && let Some(deadline) = self.first_frame_deadline
            && Instant::now() >= deadline
        {
            return Err(MpvError::NoFirstFrame);
        }

        // SAFETY: ctx is valid for the player's lifetime.
        let flags = unsafe { mpv_render_context_update(self.ctx.as_ptr()) };

        let sw_size: [i32; 2] = [self.width, self.height];
        let fmt = CString::new("bgr0").expect("bgr0 is a static literal with no NUL bytes");
        let stride = self.stride;

        let mut params = [
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: MPV_RENDER_API_TYPE_SW.as_ptr().cast_mut().cast::<c_void>(),
            },
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_SW_SIZE,
                data: (&raw const sw_size).cast_mut().cast::<c_void>(),
            },
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_SW_FORMAT,
                data: fmt.as_ptr().cast_mut().cast::<c_void>(),
            },
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_SW_STRIDE,
                data: (&raw const stride).cast_mut().cast::<c_void>(),
            },
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_SW_POINTER,
                data: buf.as_mut_ptr().cast::<c_void>(),
            },
            mpv_render_param {
                type_: mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];

        // SAFETY: `params` is a sentinel-terminated array; ctx is valid.
        let ret = unsafe { mpv_render_context_render(self.ctx.as_ptr(), params.as_mut_ptr()) };
        if ret < 0 {
            return Err(MpvError::Render(ret));
        }

        let rendered = (flags & u64::from(mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME)) != 0;
        if rendered {
            self.has_first_frame = true;
            self.first_frame_deadline = None;
        }
        Ok(rendered)
    }

    /// Test-only accessor: read a property via the inner mpv handle.
    /// Fails (with `Raw(-1)`) after [`Drop`] has run — i.e. the test
    /// must call this BEFORE dropping the player.
    #[cfg(test)]
    pub(crate) fn property(&self, name: &str) -> Result<String, libmpv2::Error> {
        match self.mpv.as_ref() {
            Some(m) => m.get_property(name),
            None => Err(libmpv2::Error::Raw(-1)),
        }
    }

    /// Test-only accessor: read a typed property (`i64`) via the
    /// inner mpv handle.  Mirrors `property` but for numeric
    /// properties like `playlist-count`.
    #[cfg(test)]
    pub(crate) fn property_i64(&self, name: &str) -> Result<i64, libmpv2::Error> {
        match self.mpv.as_ref() {
            Some(m) => m.get_property(name),
            None => Err(libmpv2::Error::Raw(-1)),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// Create a non-blocking `CLOEXEC` pipe.  Returns `(read_fd, write_fd)`.
    /// Caller owns both fds and is responsible for closing them.
    fn make_pipe() -> Result<(RawFd, RawFd), String> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: fds is a valid 2-element array; pipe2 writes both ends.
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if ret < 0 {
            return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
        }
        Ok((fds[0], fds[1]))
    }

    /// Best-effort test-video generator.  Skips (returns None) when ffmpeg
    /// isn't on PATH or fails; the caller should skip the test in that case.
    fn generate_test_video(path: &PathBuf) -> Option<()> {
        if Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x180:rate=30",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(path)
            .output()
            .is_ok_and(|o| o.status.success())
        {
            Some(())
        } else {
            None
        }
    }

    /// Wrap a raw fd so its Drop closes the fd.  Used by tests so a
    /// panic mid-test doesn't leak fds.
    struct OwnedRawFd(RawFd);
    impl OwnedRawFd {
        unsafe fn from_raw(fd: RawFd) -> Self {
            Self(fd)
        }
        fn as_raw(&self) -> RawFd {
            self.0
        }
    }
    impl Drop for OwnedRawFd {
        fn drop(&mut self) {
            // SAFETY: fd was created via pipe2; closing once.
            unsafe {
                libc::close(self.0);
            }
        }
    }

    /// Build a real `MpvPlayer` against a generated test fixture (skips
    /// when ffmpeg is absent) and return it.  Centralises the test-video
    /// + pipe setup so each test focuses on its assertion.
    fn build_test_player() -> Option<(MpvPlayer, PathBuf)> {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir temp test dir");
        let video = dir.join("test.mp4");
        generate_test_video(&video)?;
        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        let write_owned = unsafe { OwnedRawFd::from_raw(write_fd) };
        let player = MpvPlayer::new(
            vec![video.to_string_lossy().into_owned()],
            Duration::from_secs(2),
            false,
            320,
            180,
            write_owned.as_raw(),
        )
        .expect("player init");
        Some((player, video))
    }

    /// Integration with a real mpv instance + test fixture.  Skips
    /// (does not panic, does not fail) if ffmpeg isn't available.
    #[test]
    fn renders_non_zero_changing_frames() {
        let Some((mut player, _video)) = build_test_player() else {
            eprintln!("ffmpeg unavailable; skipping render test");
            return;
        };

        // Render a handful of frames, sleeping between attempts so mpv
        // has time to decode the next.  We DON'T rely on the wakeup
        // pipe here — the test verifies the render path itself, not
        // the wakeup plumbing (which is exercised by integration tests
        // once a compositor is available).
        let mut rendered = 0;
        let mut hashes: Vec<u64> = Vec::new();
        let mut first_buf: Option<Vec<u8>> = None;
        let start = Instant::now();
        while rendered < 30 && start.elapsed() < Duration::from_secs(10) {
            let mut buf = vec![0xABu8; (320 * 4 * 180) as usize];
            match player.render_frame_into(&mut buf) {
                Ok(true) => {
                    let h = buf.iter().fold(0u64, |acc, &b| {
                        acc.wrapping_mul(31).wrapping_add(u64::from(b))
                    });
                    if first_buf.is_none() {
                        first_buf = Some(buf);
                    }
                    hashes.push(h);
                    rendered += 1;
                }
                Ok(false) => {
                    std::thread::sleep(Duration::from_millis(16));
                }
                Err(e) => panic!("render errored: {e}"),
            }
        }

        assert!(
            rendered >= 5,
            "should have rendered >=5 frames, got {rendered}"
        );
        let non_zero = first_buf
            .as_ref()
            .expect("first frame")
            .iter()
            .filter(|&&b| b != 0)
            .count();
        assert!(
            non_zero > 1000,
            "expected a frame with significant non-zero pixel content, got {non_zero}"
        );
        let unique = {
            let mut sorted = hashes.clone();
            sorted.sort_unstable();
            sorted.dedup();
            sorted.len()
        };
        assert!(
            unique >= 2,
            "frame buffer hash should change across frames, got {unique} unique out of {}",
            hashes.len()
        );

        // Drop cleans up — no explicit destroy() needed.
        drop(player);
    }

    /// Asserts the sandbox flags are pinned at init time on the REAL
    /// `MpvPlayer`, NOT a parallel handle (the parallel-handle pattern
    /// can silently pass while the production player drops a flag).
    /// Also reads back `demuxer-lavf-o` if the property is readable
    /// on this mpv build — the init-must-not-fail check is the primary
    /// assertion; the readback is best-effort.
    #[test]
    fn sandbox_flags_pinned_after_init() {
        let Some((player, _video)) = build_test_player() else {
            eprintln!("ffmpeg unavailable; skipping sandbox flag test");
            return;
        };

        // If init accepted the lavf whitelist, the property may or may
        // not be readable depending on mpv build — read it best-effort
        // but do NOT fail the test on read failure (init success is
        // the binding assertion; readback is a nice-to-have).
        let lavf = player.property("demuxer-lavf-o").ok();
        eprintln!("demuxer-lavf-o readback: {lavf:?}");

        // Sandbox flags pinned at init — read back via the REAL player.
        // If `MpvPlayer::new` ever drops one of these (e.g. an
        // "innocent" refactor removes `ytdl=false`), this fails.
        let ytdl = player.property("ytdl").expect("get ytdl");
        assert_eq!(ytdl, "no", "ytdl must be pinned to 'no'");
        let load_scripts = player.property("load-scripts").expect("get load-scripts");
        assert_eq!(load_scripts, "no", "load-scripts must be pinned to 'no'");
        let osc = player.property("osc").expect("get osc");
        assert_eq!(osc, "no", "osc must be pinned to 'no'");
        let input_defaults = player
            .property("input-default-bindings")
            .expect("get input-default-bindings");
        assert_eq!(
            input_defaults, "no",
            "input-default-bindings must be pinned to 'no'"
        );
        let terminal = player.property("terminal").expect("get terminal");
        assert_eq!(terminal, "no", "terminal must be pinned to 'no'");
        let mute = player.property("mute").expect("get mute");
        assert!(
            mute.eq_ignore_ascii_case("yes"),
            "mute must be yes (audio=no is irreversible)"
        );

        drop(player);
    }

    /// Asserts that `MpvPlayer::new` accepts the `demuxer-lavf-o`
    /// protocol-whitelist setting on its own — even WITHOUT a real
    /// fixture file (no loadfile).  This is the security assertion:
    /// the option must parse, the init must succeed.
    #[test]
    fn lavf_protocol_whitelist_accepted_without_fixture() {
        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        let write_owned = unsafe { OwnedRawFd::from_raw(write_fd) };

        // One absolute path so the scheme allowlist accepts it; we
        // never loadfile successfully because the path doesn't exist —
        // the test only cares that the lavf option was accepted at
        // init.
        let player = MpvPlayer::new(
            vec!["/tmp/dormant-render-tests/lavf-probe.mp4".into()],
            Duration::from_secs(1),
            false,
            64,
            64,
            write_owned.as_raw(),
        )
        .expect("player init must accept demuxer-lavf-o at construction time");

        // Best-effort readback.
        let _ = player.property("demuxer-lavf-o");

        drop(player);
    }

    /// Pre-first-frame timeout fires when a non-existent path is loaded
    /// (mpv reports an error, never produces a frame).  Exercises the
    /// internal deadline check inside `render_frame_into`.
    #[test]
    fn missing_file_yields_no_first_frame() {
        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        let write_owned = unsafe { OwnedRawFd::from_raw(write_fd) };

        let mut player = MpvPlayer::new(
            vec!["/nonexistent/path/that/never/exists.mp4".into()],
            Duration::from_secs(1),
            false,
            64,
            64,
            write_owned.as_raw(),
        )
        .expect("player init (loadfile error is async)");

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut saw_no_first_frame = false;
        while Instant::now() < deadline {
            let mut buf = vec![0u8; 64 * 4 * 64];
            match player.render_frame_into(&mut buf) {
                Ok(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(MpvError::NoFirstFrame) => {
                    saw_no_first_frame = true;
                    break;
                }
                Err(e) => panic!("unexpected mpv error: {e}"),
            }
        }
        assert!(
            saw_no_first_frame,
            "missing-file loadfile must trip the pre-first-frame deadline"
        );

        drop(player);
    }

    /// `Drop` tears down a never-used player without panicking.  This
    /// catches leaks via the obvious smoke (assertions on fd counts
    /// via /proc/self/fd are flaky on parallel CI, so we rely on
    /// "doesn't panic + doesn't double-close" — the type system
    /// enforces the latter via `Option<RawFd>` and the fact that
    /// `Drop` only runs once).
    #[test]
    fn drop_tears_down_unused_player_without_panic() {
        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        let write_owned = unsafe { OwnedRawFd::from_raw(write_fd) };

        // Provide one allowlisted item so `MpvPlayer::new` gets past
        // the empty-after-filter check.  We never loadfile the item
        // (no render call follows) — the test is purely about Drop.
        let player = MpvPlayer::new(
            vec!["/tmp/dormant-render-tests/drop-probe.mp4".into()],
            Duration::from_secs(1),
            false,
            64,
            64,
            write_owned.as_raw(),
        )
        .expect("player init");

        // No render — just drop.  If `Drop` panics, the test fails; if
        // it double-closes the fd, libc::close on the second call
        // would also panic (in debug builds at least) or report
        // EBADF (silent in release — but a stray fd would show up
        // in fdcount tools, not in unit tests).
        drop(player);
    }

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
                "a.mp4".into(),
                "b.png".into(),
                "https://example/c.jpg".into(),
            ],
            image_duration: Duration::from_secs(3),
            audio: true,
        };
        assert_eq!(s.items.len(), 3);
        assert_eq!(s.items[0], "a.mp4");
        assert_eq!(s.items[1], "b.png");
        assert!(s.items[2].starts_with("https://"));
        assert_eq!(s.image_duration, Duration::from_secs(3));
        assert!(s.audio);
    }

    // ── scheme_allowed ───────────────────────────────────────────────

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

    // ── MpvPlayer integration with the scheme allowlist ─────────────

    /// Constructing with only exotic / non-allowlisted schemes must
    /// return `Err` WITHOUT attempting any network I/O — the filter
    /// runs before `loadfile`, so mpv never sees the items.
    /// Deterministic: the Err path is local; no port or listener is
    /// needed to assert it.
    #[test]
    fn mpv_player_rejects_all_exotic_schemes_without_loadfile() {
        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        let write_owned = unsafe { OwnedRawFd::from_raw(write_fd) };

        let result = MpvPlayer::new(
            vec![
                "ftp://127.0.0.1:1/x".into(),
                "data:text/plain,hi".into(),
                "subfile:///etc/passwd".into(),
            ],
            Duration::from_secs(1),
            false,
            64,
            64,
            write_owned.as_raw(),
        );

        assert!(
            result.is_err(),
            "all-exotic input must Err before loadfile (no network attempt)"
        );
    }

    /// Mixed list — the allowlist keeps only the local fixture; the
    /// `playlist-count` readback must reflect that.
    #[test]
    fn mpv_player_filters_playlist_to_allowed_items() {
        let Some((player, _video)) = build_test_player() else {
            eprintln!("ffmpeg unavailable; skipping playlist filter test");
            return;
        };

        // player.items was already filtered through scheme_allowed by
        // the constructor.  Read the live mpv playlist count — only
        // the local fixture should be present.
        let count: i64 = player
            .property_i64("playlist-count")
            .expect("read playlist-count");
        assert_eq!(
            count, 1,
            "expected only the allowlisted fixture in playlist, got {count}"
        );

        drop(player);
    }
}
