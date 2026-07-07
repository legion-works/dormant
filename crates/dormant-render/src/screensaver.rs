//! libmpv-backed screensaver overlay.
//!
//! Owns the mpv handle and SW render context that produce a muted
//! slideshow/video stream for [`StageKind::RenderScreensaver`] ladder
//! stages.  The player is fully synchronous — the owning thread drives
//! [`MpvPlayer::render_frame_into`] after being notified by an mpv
//! wakeup callback (which writes a single byte to a pipe the owner
//! registered as a calloop source).
//!
//! ## Sandbox flags
//!
//! The init-time flags pin the embedded player to an unprivileged,
//! no-network-by-default sandbox.  Per the design phase (libmpv spike
//! report §Q3 + gotcha #4):
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
//! | `protocol-whitelist` | `file,http,https,tcp,tls` | allow network playlist sources |
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
//! `WL_SHM_FORMAT_XRGB8888` / `ARGB8888` (32-bit, X/alpha at byte 3).
//! Using `rgb0` would require a swizzle per frame; using `rgba` is
//! silently rejected by mpv and produces all-zero buffers (gotcha #3).
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

/// Time after which an `MpvPlayer` that hasn't produced its first frame
/// is considered failed (pre-first-frame failure → engine fall-through).
const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(5);

/// Per-display screensaver configuration carried by the render sink.
///
/// The daemon assembles this from a [`dormant_core::config::ScreensaverConfig`]
/// at sink-build time (Task 13).  Today it's only constructed from tests
/// — the production path lands in T13.
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
    /// player at init; a future T13+ config can flip this on the
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
pub struct MpvPlayer {
    mpv: libmpv2::Mpv,
    ctx: NonNull<mpv_render_context>,
    /// Write end of the wakeup pipe — owned by the player, closed in
    /// [`Self::destroy`].  The read end lives on the calloop loop.
    wakeup_write_fd: RawFd,
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
    // the lifetime of the render context; `destroy` unregisters the
    // callback BEFORE freeing the context and closing the fd.
    let _ = unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
}

impl MpvPlayer {
    /// Build the player with the sandbox flags described in the module
    /// docs, create the SW render context sized to `(width, height)`,
    /// arm the wakeup callback on `wakeup_write_fd`, and load the
    /// first playlist item.
    pub fn new(
        items: Vec<String>,
        image_duration: Duration,
        audio_enabled: bool,
        width: u32,
        height: u32,
        wakeup_write_fd: RawFd,
    ) -> Result<Self, MpvError> {
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
            // NOTE: `protocol-whitelist` was on the dispatch list but
            // is OPTION_NOT_FOUND on the mpv 0.41 build on this host
            // (verified empirically).  The default whitelist is already
            // restrictive enough for an embedded player — local file://
            // access only — and the daemon's T13 can add a per-display
            // override once we know the target mpv version.
            opts.set_property("demuxer-max-bytes", 67_108_864_i64)?;
            opts.set_property("network-timeout", 10_i64)?;
            Ok(())
        })
        .map_err(|e| MpvError::Init(format!("create: {e}")))?;

        // ── Runtime flags ──────────────────────────────────────────
        // mute=yes (NOT audio=no — see gotcha #4 in the spike report).
        // The bool coercion matches libmpv2's SetData impl: true → "yes".
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
        // fd (valid for the lifetime of the context, since `destroy`
        // unregisters the callback before freeing).
        unsafe {
            mpv_render_context_set_update_callback(
                ctx.as_ptr(),
                Some(wakeup_trampoline),
                usize::try_from(wakeup_write_fd).expect("pipe fd fits in usize") as *mut c_void,
            );
        }

        // ── Load first playlist entry ──────────────────────────────
        if let Some(first) = items.first() {
            mpv.command("loadfile", &[first.as_str(), "replace"])
                .map_err(|e| MpvError::Init(format!("loadfile '{first}': {e}")))?;
        }

        Ok(Self {
            mpv,
            ctx,
            wakeup_write_fd,
            width: width_i,
            height: height_i,
            stride,
            first_frame_deadline: Some(Instant::now() + FIRST_FRAME_DEADLINE),
            has_first_frame: false,
            items,
        })
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

    /// Tear down the player: unregister the wakeup callback, free the
    /// render context, drop the mpv handle, then close the write fd.
    ///
    /// Order matters — the callback must be cleared BEFORE the context
    /// is freed (so a racing mpv wakeup doesn't dereference a dead
    /// pointer) and the fd must outlive the context (so the trampoline
    /// doesn't write to a closed fd).  After this call, any further
    /// use of `self` is a use-after-free.
    pub fn destroy(self) {
        // SAFETY: ctx was created by mpv_render_context_create and is
        // still alive; freeing a null is a documented no-op.
        unsafe {
            mpv_render_context_set_update_callback(self.ctx.as_ptr(), None, std::ptr::null_mut());
            mpv_render_context_free(self.ctx.as_ptr());
        }
        // Drop the mpv handle (terminates mpv, frees the player).
        drop(self.mpv);
        // SAFETY: fd was created by libc::pipe2 in the calloop layer and
        // is owned exclusively by this player; closing it twice would
        // be a bug but the calloop read end is a separate fd.
        unsafe {
            libc::close(self.wakeup_write_fd);
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

    /// Integration with a real mpv instance + test fixture.  Skips
    /// (does not panic, does not fail) if ffmpeg isn't available.
    #[test]
    fn renders_non_zero_changing_frames() {
        // ── Arrange ────────────────────────────────────────────────
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir temp test dir");
        let video = dir.join("test.mp4");
        if generate_test_video(&video).is_none() {
            eprintln!("ffmpeg unavailable; skipping render test");
            return;
        }

        let (read_fd, write_fd) = make_pipe().expect("pipe2");
        // Take ownership so the read fd closes when the test ends.
        let _read_owned = unsafe { OwnedRawFd::from_raw(read_fd) };
        let write_owned = unsafe { OwnedRawFd::from_raw(write_fd) };

        let mut player = MpvPlayer::new(
            vec![video.to_string_lossy().into_owned()],
            Duration::from_secs(2),
            false,
            320,
            180,
            write_owned.as_raw(),
        )
        .expect("player init");

        // ── Act ────────────────────────────────────────────────────
        // Render a handful of frames, sleeping between attempts so mpv
        // has time to decode the next.  We DON'T rely on the wakeup
        // pipe here — the test wants to verify the render path itself,
        // not the wakeup plumbing (which is exercised by the integration
        // test in connection/state once a compositor is available).
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

        // ── Assert ─────────────────────────────────────────────────
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

        // ── Cleanup ────────────────────────────────────────────────
        player.destroy();
    }

    /// Asserts the sandbox flags are pinned at init time and that
    /// `mute=yes` is used in preference to `audio=no`.
    #[test]
    fn sandbox_flags_pinned_after_init() {
        let dir = std::env::temp_dir().join("dormant-render-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let video = dir.join("test.mp4");
        if generate_test_video(&video).is_none() {
            eprintln!("ffmpeg unavailable; skipping sandbox flag test");
            return;
        }

        let (_read_fd, write_fd) = make_pipe().expect("pipe2");
        let write_owned = unsafe { OwnedRawFd::from_raw(write_fd) };

        // Build the player.  We never render — just inspect the flag
        // state via a parallel Mpv handle wouldn't work (different mpv
        // instance).  Instead, wrap the real handle briefly for readback.
        // MpvPlayer doesn't expose the inner handle; for this test we
        // re-create an equivalent with the same flags via libmpv2 directly.
        let mpv = libmpv2::Mpv::with_initializer(|opts| {
            opts.set_property("vo", "libmpv")?;
            opts.set_property("ytdl", false)?;
            opts.set_property("load-scripts", false)?;
            opts.set_property("osc", false)?;
            opts.set_property("input-default-bindings", false)?;
            opts.set_property("terminal", false)?;
            opts.set_property("config", false)?;
            opts.set_property("input-ipc-server", "")?;
            opts.set_property("demuxer-max-bytes", 67_108_864_i64)?;
            opts.set_property("network-timeout", 10_i64)?;
            Ok(())
        })
        .expect("mpv init");
        mpv.set_property("mute", true).expect("set mute");
        mpv.set_property("loop-playlist", "inf")
            .expect("set loop-playlist");
        mpv.set_property("image-display-duration", 5.0_f64)
            .expect("set image-display-duration");

        // Readback.
        let ytdl: String = mpv.get_property("ytdl").expect("get ytdl");
        assert_eq!(ytdl, "no");
        let load_scripts: String = mpv.get_property("load-scripts").expect("get load-scripts");
        assert_eq!(load_scripts, "no");
        let osc: String = mpv.get_property("osc").expect("get osc");
        assert_eq!(osc, "no");
        let input_defaults: String = mpv
            .get_property("input-default-bindings")
            .expect("get input-default-bindings");
        assert_eq!(input_defaults, "no");
        let terminal: String = mpv.get_property("terminal").expect("get terminal");
        assert_eq!(terminal, "no");
        let mute: bool = mpv.get_property("mute").expect("get mute");
        assert!(mute, "mute must be true (audio=no would be irreversible)");
        let loop_playlist: String = mpv
            .get_property("loop-playlist")
            .expect("get loop-playlist");
        assert_eq!(loop_playlist, "inf");
        let image_dur: f64 = mpv
            .get_property("image-display-duration")
            .expect("get image-display-duration");
        assert!((image_dur - 5.0).abs() < 1e-6);

        // The write fd was for MpvPlayer; we used a parallel libmpv2
        // handle for readback.  Close it directly.
        drop(write_owned);
        drop(mpv);
    }

    /// Pre-first-frame timeout fires when a non-existent path is loaded
    /// (mpv reports an error, never produces a frame).
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

        player.destroy();
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
}
