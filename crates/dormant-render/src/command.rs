//! Command types that cross the async-tokio → sync-calloop bridge.
//!
//! The render sink holds a [`calloop::channel::Sender`] that the daemon's
//! tokio task uses to enqueue [`RenderCommand`]s.  Each command carries a
//! [`tokio::sync::oneshot::Sender`] for the reply; the Wayland thread
//! delivers the result back to the awaiting tokio future.
//!
//! Kept as a pure data layer (no I/O, no Wayland types) so the encoding
//! can be exercised from unit tests without spinning up a real compositor.

use dormant_core::types::{CmdFailure, StageKind};

#[cfg(target_os = "linux")]
use crate::screensaver::ScreensaverSettings;

/// A command sent from the async engine task to the dedicated Wayland
/// thread via a [`calloop::channel`].
#[derive(Debug)]
pub(crate) enum RenderCommand {
    /// Show a render surface for the given ladder stage.  The reply
    /// resolves to `Ok(())` once the underlying surface has been committed
    /// to the compositor, or `Err(CmdFailure)` if creation/attachment
    /// failed.
    Show {
        /// Stage generation counter (matches the `r#gen` on
        /// [`dormant_core::traits::RenderSink::show`]).
        r#gen: u64,
        /// Index of the stage in the display's ladder (forwarded as-is).
        idx: usize,
        /// Stage kind: only `RenderBlack` is honoured today; any other
        /// variant yields `Err(CmdFailure)` with `E_RENDER_UNAVAILABLE`.
        kind: StageKind,
        /// Reply channel — reply is sent after the `commit()` is flushed
        /// to the compositor.
        reply: tokio::sync::oneshot::Sender<Result<(), CmdFailure>>,
    },
    /// Tear down any active render surface for this display.  Idempotent
    /// and infallible (no error variant).
    Teardown {
        /// Stage generation counter (forwarded as-is).
        r#gen: u64,
        /// Reply channel — resolves once the destruction `commit()` is
        /// flushed.
        reply: tokio::sync::oneshot::Sender<()>,
    },

    /// Linux-only: replace the current surface content with a libmpv-
    /// driven screensaver overlay.  Issued by the sink impl when the
    /// engine asks for `StageKind::RenderScreensaver` AND a screensaver
    /// config has been registered for this display.  Without a
    /// registered config the sink resolves the show with
    /// `E_RENDER_UNAVAILABLE` and never sends this command.
    ///
    /// The reply is held pending until EITHER the first successful mpv
    /// render lands (→ `Ok(())`) OR the first-frame deadline fires
    /// (→ `Err(E_RENDER_UNAVAILABLE)`).  Pre-install failures
    /// (mpv init / shm pool / calloop insert) also resolve with
    /// `Err` so the engine falls through — a missing file must NOT
    /// produce `Ok`-then-failed-to-black.  Post-first-frame failures
    /// during operation switch the surface back to the opaque-black
    /// buffer on the SAME surface (no destroy / flicker) and log
    /// `screensaver_failed_to_black`.
    #[cfg(target_os = "linux")]
    ShowScreensaver {
        /// Stage generation counter (forwarded to logs and the
        /// deadline timer's gen-guard).
        r#gen: u64,
        /// Index of the stage in the display's ladder.
        idx: usize,
        /// Screensaver config (`items` / `image_duration` / `audio`) carried
        /// with the command so the wayland thread doesn't need to look
        /// it up via a back-channel.
        settings: ScreensaverSettings,
        /// Reply channel — resolved by `on_mpv_wakeup` on first frame
        /// (`Ok`) or by the first-frame deadline timer (`Err`).
        reply: tokio::sync::oneshot::Sender<Result<(), CmdFailure>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::types::CmdFailure;

    #[test]
    fn show_command_carries_through_oneshot() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), CmdFailure>>();
        let cmd = RenderCommand::Show {
            r#gen: 7,
            idx: 2,
            kind: StageKind::RenderBlack,
            reply: tx,
        };
        match cmd {
            RenderCommand::Show {
                r#gen,
                idx,
                kind,
                reply,
            } => {
                assert_eq!(r#gen, 7);
                assert_eq!(idx, 2);
                assert_eq!(kind, StageKind::RenderBlack);
                let _ = reply.send(Ok(()));
            }
            RenderCommand::Teardown { .. } => panic!("expected Show variant"),
            #[cfg(target_os = "linux")]
            RenderCommand::ShowScreensaver { .. } => panic!("expected Show variant"),
        }
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rx)
            .unwrap();
        assert!(r.is_ok());
    }

    #[test]
    fn show_err_round_trips_through_oneshot() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), CmdFailure>>();
        let failure = CmdFailure {
            controller: "render-black".into(),
            error: "E_RENDER_UNAVAILABLE: nope".into(),
        };
        let cmd = RenderCommand::Show {
            r#gen: 1,
            idx: 0,
            kind: StageKind::RenderScreensaver,
            reply: tx,
        };
        let RenderCommand::Show { reply, .. } = cmd else {
            panic!("test expected Show variant");
        };
        let _ = reply.send(Err(failure.clone()));
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rx)
            .unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.controller, "render-black");
        assert!(err.error.starts_with("E_RENDER_UNAVAILABLE"));
    }

    #[test]
    fn teardown_command_carries_through_oneshot() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let cmd = RenderCommand::Teardown {
            r#gen: 9,
            reply: tx,
        };
        match cmd {
            RenderCommand::Teardown { r#gen, reply } => {
                assert_eq!(r#gen, 9);
                let _ = reply.send(());
            }
            RenderCommand::Show { .. } => panic!("expected Teardown variant"),
            #[cfg(target_os = "linux")]
            RenderCommand::ShowScreensaver { .. } => panic!("expected Teardown variant"),
        }
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rx)
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn show_screensaver_carries_settings() {
        use std::time::Duration;

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), CmdFailure>>();
        let settings = ScreensaverSettings {
            items: vec!["a.mp4".into(), "b.png".into()],
            image_duration: Duration::from_secs(4),
            audio: true,
        };
        let expected_items = settings.items.clone();
        let expected_dur = settings.image_duration;
        let expected_audio = settings.audio;
        let cmd = RenderCommand::ShowScreensaver {
            r#gen: 11,
            idx: 1,
            settings,
            reply: tx,
        };
        let RenderCommand::ShowScreensaver {
            r#gen,
            idx,
            settings,
            reply,
        } = cmd
        else {
            panic!("expected ShowScreensaver variant");
        };
        assert_eq!(r#gen, 11);
        assert_eq!(idx, 1);
        assert_eq!(settings.items, expected_items);
        assert_eq!(settings.image_duration, expected_dur);
        assert_eq!(settings.audio, expected_audio);
        let _ = reply.send(Ok(()));
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rx)
            .unwrap();
        assert!(r.is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn show_screensaver_err_round_trips() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), CmdFailure>>();
        let failure = CmdFailure {
            controller: "render-screensaver".into(),
            error: "E_RENDER_UNAVAILABLE: mpv init failed".into(),
        };
        let cmd = RenderCommand::ShowScreensaver {
            r#gen: 1,
            idx: 0,
            settings: ScreensaverSettings::default(),
            reply: tx,
        };
        let RenderCommand::ShowScreensaver { reply, .. } = cmd else {
            panic!("expected ShowScreensaver variant");
        };
        let _ = reply.send(Err(failure.clone()));
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rx)
            .unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.controller, "render-screensaver");
        assert!(err.error.starts_with("E_RENDER_UNAVAILABLE"));
    }
}
