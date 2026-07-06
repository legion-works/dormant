//! Command types that cross the async-tokio → sync-calloop bridge.
//!
//! The render sink holds a [`calloop::channel::Sender`] that the daemon's
//! tokio task uses to enqueue [`RenderCommand`]s.  Each command carries a
//! [`tokio::sync::oneshot::Sender`] for the reply; the Wayland thread
//! delivers the result back to the awaiting tokio future.
//!
//! Kept as a pure data layer (no I/O, no Wayland types) so the encoding
//! can be exercised from unit tests without spinning up a real compositor.

use dormant_core::types::{CmdFailure, DisplayId, StageKind};

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
    /// Configure-timeout fired for a Show still waiting on its
    /// compositor `configure` reply.  Self-resolves the pending show's
    /// oneshot with an error.
    ConfigureTimeout {
        /// Display id of the pending show — used to disambiguate if
        /// multiple sinks share the thread (not the current shape, but
        /// future-proof).
        display_id: DisplayId,
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
            RenderCommand::Teardown { .. } | RenderCommand::ConfigureTimeout { .. } => {
                panic!("expected Show variant")
            }
        }
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rx)
            .unwrap();
        assert!(r.is_ok());
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
            RenderCommand::Show { .. } | RenderCommand::ConfigureTimeout { .. } => {
                panic!("expected Teardown variant")
            }
        }
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rx)
            .unwrap();
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
    fn configure_timeout_carries_display_id() {
        let cmd = RenderCommand::ConfigureTimeout {
            display_id: DisplayId("d-1".into()),
        };
        match cmd {
            RenderCommand::ConfigureTimeout { display_id } => {
                assert_eq!(display_id.to_string(), "d-1");
            }
            _ => panic!("expected ConfigureTimeout variant"),
        }
    }
}
