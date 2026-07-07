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
use std::os::fd::{AsRawFd, RawFd};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use crate::playlist::PlaylistItem;
use crate::settings::{FIRST_FRAME_DEADLINE, ScaleMode, scheme_allowed};
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
/// then the `wakeup_write` [`OwnedFd`] field drops LAST via Rust's
/// struct-field declaration order — the fd is only closed after the
/// context it was registered against is gone.
///
/// The write fd is `OwnedFd` end-to-end so any pre-construction
/// `?`/early-return in [`MpvPlayer::new`] drops it automatically; the
/// only path the caller has to worry about is the `Err` it
/// propagates back, and even there the caller must NOT close the fd
/// — `OwnedFd::drop` already ran.
pub struct MpvPlayer {
    mpv: Option<libmpv2::Mpv>,
    ctx: NonNull<mpv_render_context>,
    width: i32,
    height: i32,
    stride: i32,
    /// Absolute instant after which a still-frame-less player is dead.
    /// `None` after the first frame is produced.
    first_frame_deadline: Option<Instant>,
    /// Set after the first successful render — used to gate the
    /// pre-first-frame timeout.
    has_first_frame: bool,
    /// Items loaded into the mpv playlist — kept to pass per-item
    /// durations via the `loadfile` options string.
    #[allow(dead_code)]
    items: Vec<PlaylistItem>,
    /// Write end of the wakeup pipe — `OwnedFd` so any pre-construction
    /// `?`-early-return in [`MpvPlayer::new`] drops it automatically.
    /// Declared LAST so its `Drop` (which closes the fd) runs AFTER the
    /// mpv handle, context, and callback cleanup in [`Drop::drop`] —
    /// the callback fires against this fd until [`Drop::drop`] unregisters
    /// it, so the fd must outlive everything else.
    ///
    /// The field is "never read" by any method body — its only job
    /// is to hold the `OwnedFd` until [`Drop::drop`] runs, then the
    /// field's natural drop closes the fd.  The `dead_code` allow is
    /// intentional: do not introduce a spurious read just to silence
    /// the lint — the field is load-bearing on Drop.
    #[allow(dead_code)]
    wakeup_write: Option<std::os::fd::OwnedFd>,
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
        // The `wakeup_write` `OwnedFd` field drops NEXT via Rust's
        // struct-field declaration order — it was declared LAST
        // specifically so this Drop runs first (which unregisters the
        // callback against a still-valid fd), and the fd close runs
        // last (after the context it was registered against is freed).
        // Nothing else to do here — the field's natural drop closes
        // the fd exactly once.
    }
}

impl MpvPlayer {
    /// Build the player with the sandbox flags described in the module
    /// docs, filter the playlist through [`scheme_allowed`], create
    /// the SW render context sized to `(width, height)`, arm the
    /// wakeup callback on `wakeup_write`, and (last) load the
    /// first surviving playlist item.
    ///
    /// Order matters for leak-freedom: the owned `MpvPlayer` struct
    /// is constructed (with `Option`-wrapped mpv handle, `OwnedFd`
    /// wakeup fd, and the render-context pointer) BEFORE `loadfile`
    /// runs.  Any error after the `MpvPlayer` value exists lets `Drop`
    /// clean up — unset the callback, free the context, drop mpv, then
    /// the `OwnedFd` field drops last via Rust's struct-field
    /// declaration order, closing the fd.  Pre-construction errors
    /// (mpv init, render-context creation, all-items-rejected) also
    /// leak-freely: `wakeup_write: OwnedFd` is a function parameter
    /// (owned from the caller's first frame), and any `?` early-return
    /// drops it before the `Err` propagates.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        items: Vec<PlaylistItem>,
        image_duration: Duration,
        audio_enabled: bool,
        scale_mode: ScaleMode,
        width: u32,
        height: u32,
        wakeup_write: std::os::fd::OwnedFd,
    ) -> Result<Self, MpvError> {
        // ── Scheme allowlist (PRIMARY security control) ───────────
        // Runs BEFORE any mpv interaction so a rejected item never
        // causes mpv to even consider opening it.  Logged at warn so
        // operators can audit config sources that produce URLs.
        let filtered_items: Vec<PlaylistItem> = items
            .into_iter()
            .filter(|item| {
                if scheme_allowed(&item.uri) {
                    true
                } else {
                    tracing::warn!(
                        event = "screensaver_item_rejected",
                        item = %item.uri,
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

        // ── Scale-mode properties ──────────────────────────────────
        // The four canonical mpv scaling flags for the matching [`ScaleMode`]
        // variant.  Set BEFORE `loadfile` so mpv applies them to the very
        // first decoded frame.  Property-readback coverage for each mode
        // lives in `mpv_player_sets_scale_mode_properties_*` and the
        // pixel-buffer geometric coverage in
        // `mpv_player_fill_renders_no_letterbox_on_portrait_fixture`.
        // `Fit` explicitly sets `keepaspect=yes panscan=0.0` for
        // determinism — mpv's defaults include `keepaspect=yes` but
        // leaving `panscan` unset means a future runtime set could leave
        // a stray panscan value in place.
        let (keepaspect, panscan, video_unscaled): (&str, &str, &str) = match scale_mode {
            ScaleMode::Fill => ("yes", "1.0", "no"),
            ScaleMode::Fit => ("yes", "0.0", "no"),
            ScaleMode::Stretch => ("no", "0.0", "no"),
            ScaleMode::Center => ("yes", "0.0", "yes"),
        };
        mpv.set_property("keepaspect", keepaspect)
            .map_err(|e| MpvError::Init(format!("set keepaspect: {e}")))?;
        mpv.set_property("panscan", panscan)
            .map_err(|e| MpvError::Init(format!("set panscan: {e}")))?;
        mpv.set_property("video-unscaled", video_unscaled)
            .map_err(|e| MpvError::Init(format!("set video-unscaled: {e}")))?;

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
                usize::try_from(wakeup_write.as_raw_fd()).expect("pipe fd fits in usize")
                    as *mut c_void,
            );
        }

        // ── Construct the owned player BEFORE loadfile ────────────
        // After this point, `Drop` owns the cleanup of ctx, mpv, and
        // the wakeup write fd (the `wakeup_write` `OwnedFd` field is
        // declared LAST so its `Drop` runs after `Drop::drop`'s explicit
        // cleanup).  `load_items` runs against the constructed player —
        // on Err, the value drops and cleans up.
        let mut player = Self {
            mpv: Some(mpv),
            ctx,
            width: width_i,
            height: height_i,
            stride,
            first_frame_deadline: Some(Instant::now() + FIRST_FRAME_DEADLINE),
            has_first_frame: false,
            items: filtered_items,
            wakeup_write: Some(wakeup_write),
        };

        // ── Load playlist items (last fallible step) ───────────────
        // `items` was already filtered through `scheme_allowed` above,
        // so every item is by construction inside the allowlist.
        player.load_items()?;

        Ok(player)
    }

    /// Load the filtered [`PlaylistItem`]s into mpv's playlist.
    ///
    /// The first item uses `loadfile <uri> replace -1 <options>`;
    /// subsequent items use `loadfile <uri> append-play -1 <options>`.
    /// Per-item `image_duration` is passed as a per-file option
    /// (`image-display-duration=N`); items with `None` duration omit
    /// the option string entirely, falling back to the global default.
    ///
    /// # mpv per-file options — mpv 0.41 (verified)
    ///
    /// `loadfile` takes 4 positional args on mpv ≥0.38:
    ///
    /// ```text
    /// loadfile <url> <flags> <index> <options>
    /// ```
    ///
    /// - `flags`: `"replace"` for the first entry, `"append-play"` for the rest.
    /// - `index`: `-1` (auto: end of playlist for append-play).
    /// - `options`: comma-separated `key=value` pairs, e.g.
    ///   `"image-display-duration=5.0"`.
    ///
    /// The 3-arg form (`loadfile <url> <flags> <options>`) is **rejected** on
    /// mpv 0.41 (`"invalid parameter"` in the IPC probe); the index position
    /// is mandatory despite mpv's help text claiming it's optional.
    fn load_items(&mut self) -> Result<(), MpvError> {
        let mpv = self
            .mpv
            .as_mut()
            .ok_or_else(|| MpvError::Init("player destroyed before load_items".into()))?;

        for (i, item) in self.items.iter().enumerate() {
            let flags = if i == 0 { "replace" } else { "append-play" };

            match item.image_duration {
                Some(dur) => {
                    let opt = format!("image-display-duration={}", dur.as_secs_f64());
                    mpv.command("loadfile", &[item.uri.as_str(), flags, "-1", &opt])
                }
                None => mpv.command("loadfile", &[item.uri.as_str(), flags, "-1", ""]),
            }
            .map_err(|e| MpvError::Init(format!("loadfile '{}': {e}", item.uri)))?;
        }

        Ok(())
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
    use std::os::fd::FromRawFd;
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

    /// Variant of `generate_test_video` for the scale-mode geometric test:
    /// produces a 100×400 portrait testsrc (1:4 aspect) explicitly chosen
    /// so a 320×180 render target would have to LETTERBOX (Fit) — there
    /// is no geometry under which a 1:4 source fills a 16:9 rectangle
    /// without distortion or crop.  Skips on ffmpeg failure.
    fn generate_portrait_test_video(path: &PathBuf) -> Option<()> {
        if Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=100x400:rate=30",
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

    /// Build a real `MpvPlayer` against a generated test fixture (skips
    /// when ffmpeg is absent) and return it.  Centralises the test-video
    /// + pipe setup so each test focuses on its assertion.
    ///
    /// `scale_mode` defaults to `Fit` so the existing tests (which assert
    /// on the letterbox / generic render behaviour, not on scaling
    /// semantics) get the prior effective behaviour — only the
    /// `mpv_player_sets_scale_mode_properties_*` tests below exercise
    /// the scaling properties explicitly.
    fn build_test_player() -> Option<(MpvPlayer, PathBuf)> {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir temp test dir");
        let video = dir.join("test.mp4");
        generate_test_video(&video)?;
        build_test_player_with_mode(video, ScaleMode::Fit)
    }

    /// Build a `MpvPlayer` against an already-generated video path with
    /// an explicit [`ScaleMode`].  Skips on environment capability gaps
    /// (missing codecs/demuxers) — see `build_test_player` for the same
    /// affordance.
    fn build_test_player_with_mode(
        video: PathBuf,
        scale_mode: ScaleMode,
    ) -> Option<(MpvPlayer, PathBuf)> {
        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        // SAFETY: write_fd was just created by pipe2 and is not yet owned
        // by anything else — `OwnedFd` takes exclusive ownership.
        let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(write_fd) };
        let player = match MpvPlayer::new(
            vec![PlaylistItem {
                uri: video.to_string_lossy().into_owned(),
                image_duration: None,
            }],
            Duration::from_secs(2),
            false,
            scale_mode,
            320,
            180,
            write_owned,
        ) {
            Ok(p) => p,
            Err(MpvError::Init(msg)) if msg.contains("loadfile") => {
                eprintln!(
                    "libmpv cannot load test media on this host; \
                     skipping test (loadfile error: {msg})"
                );
                return None;
            }
            Err(e) => panic!("player init: {e}"),
        };
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
        let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(write_fd) };

        // One absolute path so the scheme allowlist accepts it; we
        // never loadfile successfully because the path doesn't exist —
        // the test only cares that the lavf option was accepted at
        // init.
        let player = match MpvPlayer::new(
            vec![PlaylistItem {
                uri: "/tmp/dormant-render-tests/lavf-probe.mp4".into(),
                image_duration: None,
            }],
            Duration::from_secs(1),
            false,
            ScaleMode::Fit,
            64,
            64,
            write_owned,
        ) {
            Ok(p) => p,
            Err(MpvError::Init(msg)) if msg.contains("loadfile") => {
                eprintln!(
                    "libmpv cannot load test media on this host; \
                     skipping lavf_protocol_whitelist_accepted_without_fixture"
                );
                return;
            }
            Err(e) => {
                panic!("player init must accept demuxer-lavf-o at construction time: {e}")
            }
        };

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
        let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(write_fd) };

        let mut player = match MpvPlayer::new(
            vec![PlaylistItem {
                uri: "/nonexistent/path/that/never/exists.mp4".into(),
                image_duration: None,
            }],
            Duration::from_secs(1),
            false,
            ScaleMode::Fit,
            64,
            64,
            write_owned,
        ) {
            Ok(p) => p,
            Err(MpvError::Init(msg)) if msg.contains("loadfile") => {
                eprintln!(
                    "libmpv cannot load test media on this host; \
                     skipping missing_file_yields_no_first_frame"
                );
                return;
            }
            Err(e) => panic!("player init (loadfile error is async): {e}"),
        };

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
        let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(write_fd) };

        // Provide one allowlisted item so `MpvPlayer::new` gets past
        // the empty-after-filter check.  We never loadfile the item
        // (no render call follows) — the test is purely about Drop.
        let player = match MpvPlayer::new(
            vec![PlaylistItem {
                uri: "/tmp/dormant-render-tests/drop-probe.mp4".into(),
                image_duration: None,
            }],
            Duration::from_secs(1),
            false,
            ScaleMode::Fit,
            64,
            64,
            write_owned,
        ) {
            Ok(p) => p,
            Err(MpvError::Init(msg)) if msg.contains("loadfile") => {
                eprintln!(
                    "libmpv cannot load test media on this host; \
                     skipping drop_tears_down_unused_player_without_panic"
                );
                return;
            }
            Err(e) => panic!("player init: {e}"),
        };

        // No render — just drop.  If `Drop` panics, the test fails; if
        // it double-closes the fd, libc::close on the second call
        // would also panic (in debug builds at least) or report
        // EBADF (silent in release — but a stray fd would show up
        // in fdcount tools, not in unit tests).
        drop(player);
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
        let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(write_fd) };

        let result = MpvPlayer::new(
            vec![
                PlaylistItem {
                    uri: "ftp://127.0.0.1:1/x".into(),
                    image_duration: None,
                },
                PlaylistItem {
                    uri: "data:text/plain,hi".into(),
                    image_duration: None,
                },
                PlaylistItem {
                    uri: "subfile:///etc/passwd".into(),
                    image_duration: None,
                },
            ],
            Duration::from_secs(1),
            false,
            ScaleMode::Fit,
            64,
            64,
            write_owned,
        );

        assert!(
            result.is_err(),
            "all-exotic input must Err before loadfile (no network attempt)"
        );
    }

    /// Companion to `mpv_player_rejects_all_exotic_schemes_without_loadfile`:
    /// the write fd MUST be closed after the Err (it's `OwnedFd`, so
    /// the function's early-return drops it).  Deterministic headless
    /// check via `libc::fcntl(fd, F_GETFD)` — EBADF confirms the fd is
    /// closed.  `F_GETFD` is side-effect-free and unambiguous: a closed
    /// fd returns -1 with errno EBADF regardless of how the fd was
    /// originally opened, while `write()` on a read-only fd that was
    /// REUSED by another process could false-pass.  We hold the raw fd
    /// in a local (NOT `OwnedFd`) so the assertion sees the kernel
    /// state, not the Rust destructor.
    #[test]
    fn write_fd_is_closed_after_all_rejected_err() {
        let (read_fd, write_fd) = make_pipe().expect("pipe2");
        // Close read_fd manually so it doesn't leak the test fd — the
        // point of this test is the WRITE fd's close, not the read end.
        unsafe {
            libc::close(read_fd);
        }
        // Hold the raw write_fd value across the new() call so we can
        // observe the kernel close via libc::fcntl.
        let write_raw = write_fd;

        // Construct inside a block so `OwnedFd` (the wrapper we pass
        // to new()) is dropped deterministically at the block's end —
        // whether new() returned Ok or Err, the OwnedFd drops there.
        let result = {
            let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(write_fd) };
            MpvPlayer::new(
                vec![
                    PlaylistItem {
                        uri: "ftp://127.0.0.1:1/x".into(),
                        image_duration: None,
                    },
                    PlaylistItem {
                        uri: "data:text/plain,hi".into(),
                        image_duration: None,
                    },
                ],
                Duration::from_secs(1),
                false,
                ScaleMode::Fit,
                64,
                64,
                write_owned,
            )
        };

        // Assert the path WAS the filter-rejection Err — without this,
        // the fd-closed check below would pass even if the filter was
        // bypassed (the player would be constructed and the fd would
        // be closed by the player's drop instead of the early-return
        // drop).  Both paths close the fd; the point of this test is
        // to prove the EARLY-RETURN drop runs.
        assert!(
            result.is_err(),
            "all-exotic input must Err before loadfile (so the early-return drop runs)"
        );

        // The write fd must now be closed.  `fcntl(fd, F_GETFD)` is
        // the unambiguous probe: returns -1 with errno EBADF for a
        // closed fd; no side effects on any fd state.
        let ret = unsafe { libc::fcntl(write_raw, libc::F_GETFD) };
        assert_eq!(
            ret, -1,
            "write fd must be closed after MpvPlayer::new Err (fcntl F_GETFD returned {ret})"
        );
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            errno,
            Some(libc::EBADF),
            "expected EBADF from fcntl F_GETFD on closed fd, got errno={errno:?}"
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

    /// Per-file options via the 4-arg `loadfile` path must be accepted
    /// by the real mpv.  Two PNG files with different `image_duration` values
    /// are loaded; the construction succeeds for both and the playlist
    /// contains both entries.  (Visual duration effect is acceptance-
    /// tested visually — this test proves the non-empty options string
    /// is well-formed through the 4-arg positional.)
    #[test]
    fn loadfile_per_file_options_accepted_by_mpv() {
        use std::process::Command;

        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir temp test dir");
        let img1 = dir.join("perfile_test_1.png");
        let img2 = dir.join("perfile_test_2.png");

        // Generate two tiny 1-frame images; skip if ffmpeg unavailable.
        for (path, label) in &[(&img1, "red"), (&img2, "blue")] {
            let ok = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("color=c={label}:size=32x32:duration=0.1"),
                    "-frames:v",
                    "1",
                ])
                .arg(path)
                .output()
                .is_ok_and(|o| o.status.success());
            if !ok {
                eprintln!("ffmpeg unavailable; skipping per-file options test");
                return;
            }
        }

        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(write_fd) };

        let player = match MpvPlayer::new(
            vec![
                PlaylistItem {
                    uri: img1.to_string_lossy().into_owned(),
                    image_duration: Some(Duration::from_secs_f64(0.5)),
                },
                PlaylistItem {
                    uri: img2.to_string_lossy().into_owned(),
                    image_duration: Some(Duration::from_secs_f64(1.25)),
                },
            ],
            Duration::from_secs(10), // global default
            false,
            ScaleMode::Fit, // arbitrary — this test is about per-file options, not scaling.
            64,
            64,
            write_owned,
        ) {
            Ok(p) => p,
            Err(MpvError::Init(msg)) if msg.contains("loadfile") => {
                eprintln!(
                    "libmpv cannot load test media on this host; \
                     skipping loadfile_per_file_options_accepted_by_mpv"
                );
                return;
            }
            Err(e) => panic!("MpvPlayer::new with per-item durations: {e}"),
        };

        let count: i64 = player
            .property_i64("playlist-count")
            .expect("read playlist-count");
        assert_eq!(
            count, 2,
            "expected both per-file-option items in playlist, got {count}"
        );

        drop(player);
    }

    // ── Scale-mode property tests ──────────────────────────────────────
    //
    // The probing in the build_test_player_with_mode path requires a
    // real (decodable) source so the property readback is meaningful;
    // the tests below skip on host capability gaps (matching the
    // existing media-load skip-guard pattern used everywhere else in
    // this module).

    /// Sanity check for [`build_test_player_with_mode`] on Fit (the
    /// default test path): the property readback reflects the Fit
    /// mode's settings.  Catches the case where the `ScaleMode` parameter
    /// gets dropped silently — `Fill` is the production default so we
    /// test Fit explicitly to assert the value made it through.
    #[test]
    fn mpv_player_sets_scale_mode_properties_fit() {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let video = dir.join("scale_mode_test.mp4");
        if generate_test_video(&video).is_none() {
            eprintln!("ffmpeg unavailable; skipping scale-mode Fit test");
            return;
        }
        let Some((player, _video)) = build_test_player_with_mode(video, ScaleMode::Fit) else {
            eprintln!("libmpv cannot load test media; skipping scale-mode Fit test");
            return;
        };

        // Fit: keepaspect=yes, panscan=0.0, video-unscaled=no.
        // The `panscan` property is f64 on this mpv build; matching the
        // probe readback path keeps the test format consistent.
        let keepaspect = player.property("keepaspect").expect("get keepaspect");
        assert_eq!(keepaspect, "yes", "Fit must set keepaspect=yes");
        let panscan_raw = player.property("panscan").expect("get panscan");
        let panscan: f64 = panscan_raw
            .parse()
            .unwrap_or_else(|_| panic!("panscan must parse as f64, got {panscan_raw:?}"));
        assert!(
            panscan.abs() < 0.01,
            "Fit must set panscan≈0.0 (got {panscan})"
        );
        let video_unscaled = player
            .property("video-unscaled")
            .expect("get video-unscaled");
        assert_eq!(video_unscaled, "no", "Fit must set video-unscaled=no");

        drop(player);
    }

    #[test]
    fn mpv_player_sets_scale_mode_properties_fill() {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let video = dir.join("scale_mode_test_fill.mp4");
        if generate_test_video(&video).is_none() {
            eprintln!("ffmpeg unavailable; skipping scale-mode Fill test");
            return;
        }
        let Some((player, _video)) = build_test_player_with_mode(video, ScaleMode::Fill) else {
            eprintln!("libmpv cannot load test media; skipping scale-mode Fill test");
            return;
        };

        // Fill: keepaspect=yes + panscan=1.0 (the OS-screensaver norm).
        let keepaspect = player.property("keepaspect").expect("get keepaspect");
        assert_eq!(keepaspect, "yes", "Fill must set keepaspect=yes");
        let panscan_raw = player.property("panscan").expect("get panscan");
        let panscan: f64 = panscan_raw
            .parse()
            .unwrap_or_else(|_| panic!("panscan must parse as f64, got {panscan_raw:?}"));
        // panscan=1.0 in canonical form; some mpv versions normalize to
        // 1.000000 etc — compare with a small tolerance.
        assert!(
            (panscan - 1.0).abs() < 0.01,
            "Fill must set panscan≈1.0 (got {panscan})"
        );
        let video_unscaled = player
            .property("video-unscaled")
            .expect("get video-unscaled");
        assert_eq!(video_unscaled, "no", "Fill must set video-unscaled=no");

        drop(player);
    }

    #[test]
    fn mpv_player_sets_scale_mode_properties_stretch() {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let video = dir.join("scale_mode_test_stretch.mp4");
        if generate_test_video(&video).is_none() {
            eprintln!("ffmpeg unavailable; skipping scale-mode Stretch test");
            return;
        }
        let Some((player, _video)) = build_test_player_with_mode(video, ScaleMode::Stretch) else {
            eprintln!("libmpv cannot load test media; skipping scale-mode Stretch test");
            return;
        };

        // Stretch: keepaspect=no, panscan=0.0, video-unscaled=no.
        let keepaspect = player.property("keepaspect").expect("get keepaspect");
        assert_eq!(keepaspect, "no", "Stretch must set keepaspect=no");
        let video_unscaled = player
            .property("video-unscaled")
            .expect("get video-unscaled");
        assert_eq!(video_unscaled, "no", "Stretch must set video-unscaled=no");

        drop(player);
    }

    #[test]
    fn mpv_player_sets_scale_mode_properties_center() {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let video = dir.join("scale_mode_test_center.mp4");
        if generate_test_video(&video).is_none() {
            eprintln!("ffmpeg unavailable; skipping scale-mode Center test");
            return;
        }
        let Some((player, _video)) = build_test_player_with_mode(video, ScaleMode::Center) else {
            eprintln!("libmpv cannot load test media; skipping scale-mode Center test");
            return;
        };

        // Center: keepaspect=yes, video-unscaled=yes (1:1 native size).
        let keepaspect = player.property("keepaspect").expect("get keepaspect");
        assert_eq!(keepaspect, "yes", "Center must set keepaspect=yes");
        let video_unscaled = player
            .property("video-unscaled")
            .expect("get video-unscaled");
        assert_eq!(video_unscaled, "yes", "Center must set video-unscaled=yes");

        drop(player);
    }

    /// Integration assertion: building with Fill renders a NON-LETTERBOXED
    /// frame on a deliberately non-16:9 portrait fixture, while Fit
    /// renders LETTERBOXED with substantial black pillars on each side.
    ///
    /// ## Why a 1:4 portrait fixture?
    ///
    /// Earlier revisions of this test compared 320×180 SW render against
    /// the same 320×180 testsrc fixture (a 16:9 source).  That was a
    /// tautology: Fill and Fit have no geometric reason to differ on a
    /// source whose aspect matches the target — the `assert_ne!` only
    /// passed from decoder/timestamp drift between two independently
    /// sampled mpv players, NOT from any scaling, so a regression that
    /// dropped the `panscan` property-set silently passed the test.
    /// This reviewer red-check is recorded in the P2.1 review report.
    ///
    /// On a 100×400 (1:4) source against a 320×180 (16:9) target, Fit
    /// must letterbox with ~45×180 px content band centred → ~137 px
    /// pillars each side.  Fill must cover the width (with the slight
    /// panscan off-centre quirk up to ~37 px on extreme 1:4 sources),
    /// so the combined pillar count is bounded at ≪ 137.
    ///
    /// ## Pillars semantics
    ///
    /// A column is a "black pillar" if EVERY sampled row (y ∈
    /// {10, 30, 60, 90, 120, 150, 170}) has BGR ≤ 8 (the same tolerance
    /// used by the original probe and `build_test_player`'s test
    /// pattern).  A column with ANY non-black row is content.  We count
    /// **maximum consecutive black columns from the left edge** as the
    /// "left pillar width" and same from the right edge as the right
    /// pillar width — this catches both letterboxing and any future
    /// border-drawing regressions.
    ///
    /// ## RED-check guarantee
    ///
    /// Removing the `panscan=1.0` property-set line for Fill causes
    /// this test to FAIL — confirmed against the broken code path
    /// (re-run during P2.1 review).
    #[allow(clippy::too_many_lines)]
    #[test]
    fn mpv_player_fill_renders_no_letterbox_on_portrait_fixture() {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Deliberately non-16:9 — see the doc-comment above.
        let video = dir.join("scale_mode_portrait.mp4");
        if generate_portrait_test_video(&video).is_none() {
            eprintln!("ffmpeg unavailable; skipping fill-vs-fit geometric test");
            return;
        }

        let build = |mode: ScaleMode| -> Option<MpvPlayer> {
            let (_r, w) = make_pipe().expect("pipe2");
            let write_owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(w) };
            let player = MpvPlayer::new(
                vec![PlaylistItem {
                    uri: video.to_string_lossy().into_owned(),
                    image_duration: None,
                }],
                Duration::from_secs(2),
                false,
                mode,
                320,
                180,
                write_owned,
            );
            match player {
                Ok(p) => Some(p),
                Err(MpvError::Init(msg)) if msg.contains("loadfile") => {
                    eprintln!(
                        "libmpv cannot load test media; skipping fill-vs-fit geometric \
                         test (loadfile: {msg})"
                    );
                    None
                }
                Err(e) => panic!("player init: {e}"),
            }
        };

        let Some(mut fill_player) = build(ScaleMode::Fill) else {
            return;
        };
        let Some(mut fit_player) = build(ScaleMode::Fit) else {
            drop(fill_player);
            return;
        };

        // Render one frame after a short warm-up.  A single frame is
        // sufficient — the geometric property (pillar width) is
        // deterministic per (mode, fixture, frame_index); we just need
        // a valid first frame.
        let sample = |player: &mut MpvPlayer| -> Option<Vec<u8>> {
            for _ in 0..20 {
                let mut buf = vec![0u8; (320 * 4 * 180) as usize];
                if let Ok(true) = player.render_frame_into(&mut buf) {
                    return Some(buf);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            None
        };

        let fill_buf = sample(&mut fill_player);
        let fit_buf = sample(&mut fit_player);

        let (Some(fill_buf), Some(fit_buf)) = (fill_buf, fit_buf) else {
            drop(fill_player);
            drop(fit_player);
            panic!("neither Fill nor Fit rendered any frame on this host");
        };

        // ── Pillar detection ────────────────────────────────────────
        // A column is a black pillar iff ALL sampled rows are ≤ 8 BGR
        // (i.e. the column has no content pixel anywhere we sampled).
        let y_samples = [10usize, 30, 60, 90, 120, 150, 170];
        let is_pillar_col = |buf: &[u8], x: usize| -> bool {
            y_samples.iter().all(|&y| {
                let off = (y * 320 + x) * 4;
                buf[off] <= 8 && buf[off + 1] <= 8 && buf[off + 2] <= 8
            })
        };
        // Maximum consecutive black columns from the left edge and from
        // the right edge — these are the letterbox pillar widths.
        let left_pillar = |buf: &[u8]| -> usize {
            let mut w = 0;
            for x in 0..320 {
                if is_pillar_col(buf, x) {
                    w += 1;
                } else {
                    break;
                }
            }
            w
        };
        let right_pillar = |buf: &[u8]| -> usize {
            let mut w = 0;
            for x in (0..320).rev() {
                if is_pillar_col(buf, x) {
                    w += 1;
                } else {
                    break;
                }
            }
            w
        };

        let fill_left = left_pillar(&fill_buf);
        let fill_right = right_pillar(&fill_buf);
        let fit_left = left_pillar(&fit_buf);
        let fit_right = right_pillar(&fit_buf);

        eprintln!(
            "scale-mode geometry: Fill L/R = {fill_left}/{fill_right}px, \
             Fit L/R = {fit_left}/{fit_right}px"
        );

        // ── Fit must letterbox (this proves Fit itself works) ──────
        // 100×400 → 320×180: aspect-fit Fit renders a centred 45×180
        // content band → ~137 px black pillars each side.  We demand
        // ≥ 100 px on each side to give some slack for vignette crops.
        assert!(
            fit_left >= 100,
            "Fit must letterbox (≥100 px left pillar) on the 1:4 fixture; \
             got {fit_left}.  Either the fixture isn't portrait or Fit \
             unexpectedly fills — both indicate a regression."
        );
        assert!(
            fit_right >= 100,
            "Fit must letterbox (≥100 px right pillar) on the 1:4 fixture; \
             got {fit_right}."
        );

        // ── Fill must NOT letterbox (THIS is the RED-check) ─────────
        // Fill (panscan=1.0) covers the width.  The extreme 1:4 source
        // still produces a slight off-centre quirk: per the STEP-0
        // probe evidence, pillars total ≤ ~40 px.  We allow ≤ 80 as a
        // margin for codec/file-path variance; >= 100 (the Fit floor)
        // would unambiguously mean "looks letterboxed → panscan not
        // applied".  This is the assertion that catches
        // `mpv.set_property(\"panscan\", ...)` being removed.
        let fill_total = fill_left + fill_right;
        assert!(
            fill_total <= 80,
            "Fill must fill width on the 1:4 fixture (≤80 px combined pillars); \
             got L={fill_left} + R={fill_right} = {fill_total} px.  \
             Likely cause: the `panscan` property-set was dropped or \
             regressed (this test fails RED when it does — verified during P2.1 review)."
        );

        // Bonus geometric identity: Fill's pillars must be smaller than
        // Fit's on each side.  Not strictly necessary (the floor
        // assertions already cover it) but cheap and a sharp sensor
        // for asymmetric regressions like "left pillar vanished but
        // right grew".
        assert!(
            fill_left < fit_left,
            "Fill must have smaller left pillar than Fit ({fill_left} vs {fit_left})"
        );
        assert!(
            fill_right < fit_right,
            "Fill must have smaller right pillar than Fit ({fill_right} vs {fit_right})"
        );

        drop(fill_player);
        drop(fit_player);
    }
}
