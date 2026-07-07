//! Wayland layer-shell [`RenderSink`] implementation.
//!
//! See the crate-root module docs for the thread model.  This file
//! owns the public [`LayerShellRenderSink`] handle and threads the
//! show/teardown commands to the dedicated wayland thread spawned by
//! [`connection::spawn_wayland_thread`].

mod connection;
mod state;
mod surface;

use std::fmt;

use async_trait::async_trait;
use calloop::channel::Sender;
use tokio::sync::mpsc::UnboundedSender;

use dormant_core::error::E_RENDER_UNAVAILABLE;
use dormant_core::traits::RenderSink;
use dormant_core::types::{CmdFailure, DisplayId, StageKind};

use crate::command::RenderCommand;

/// Per-display handle that ships [`RenderSink`] commands across the
/// async-tokio → sync-calloop boundary to a dedicated wayland thread.
///
/// `Clone`-able (the handle holds a small set of `Clone` channels);
/// the actual Wayland objects live on the thread and are inaccessible
/// from any other thread.
#[derive(Clone)]
pub struct LayerShellRenderSink {
    display_id: DisplayId,
    output_name: String,
    cmd_tx: Sender<RenderCommand>,
}

impl fmt::Debug for LayerShellRenderSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerShellRenderSink")
            .field("display_id", &self.display_id)
            .field("output_name", &self.output_name)
            .finish_non_exhaustive()
    }
}

impl LayerShellRenderSink {
    /// Build a per-display sink.  Spawns a dedicated wayland thread that
    /// owns the compositor connection, performs the initial bind, and
    /// drives a calloop event loop.
    ///
    /// # Errors
    ///
    /// Returns `Err(CmdFailure{ controller: "render-black", .. })` with a
    /// `E_RENDER_UNAVAILABLE` prefix when:
    ///
    /// - the process has no `WAYLAND_DISPLAY` socket,
    /// - the compositor doesn't advertise `zwlr_layer_shell_v1`,
    /// - the requested output connector name is not present, or
    /// - the OS thread cannot be spawned.
    ///
    /// # `input_wake_tx`
    ///
    /// When set, the first pointer / key event on the active surface
    /// pushes the display id through this channel.  The daemon wires
    /// it up to `ControlMsg::InputWake` so the engine can route a wake.
    /// When `None`, the surface is still rendered but wake events are
    /// dropped.
    pub fn new(
        display_id: DisplayId,
        output_name: String,
        input_wake_tx: Option<&UnboundedSender<DisplayId>>,
    ) -> Result<Self, CmdFailure> {
        let cmd_tx = connection::spawn_wayland_thread(&display_id, &output_name, input_wake_tx)?;
        Ok(Self {
            display_id,
            output_name,
            cmd_tx,
        })
    }

    /// Identifier of the display this sink was built for.
    #[must_use]
    pub fn display_id(&self) -> &DisplayId {
        &self.display_id
    }

    /// Connector name of the output this sink was built for.
    #[must_use]
    pub fn output_name(&self) -> &str {
        &self.output_name
    }
}

#[async_trait]
impl RenderSink for LayerShellRenderSink {
    async fn show(&self, r#gen: u64, idx: usize, kind: StageKind) -> Result<(), CmdFailure> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Result<(), CmdFailure>>();
        self.cmd_tx
            .send(RenderCommand::Show {
                r#gen,
                idx,
                kind,
                reply: reply_tx,
            })
            .map_err(|_send_err| CmdFailure {
                controller: "render-black".into(),
                error: format!("{E_RENDER_UNAVAILABLE}: wayland thread not running"),
            })?;
        reply_rx.await.map_err(|_recv_err| CmdFailure {
            controller: "render-black".into(),
            error: format!("{E_RENDER_UNAVAILABLE}: wayland thread dropped reply"),
        })?
    }

    async fn teardown(&self, r#gen: u64) {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<()>();
        // Sending failures are non-fatal for `teardown` — the surface
        // will eventually time out / be reaped.  Swallow.
        if self
            .cmd_tx
            .send(RenderCommand::Teardown {
                r#gen,
                reply: reply_tx,
            })
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }
}
