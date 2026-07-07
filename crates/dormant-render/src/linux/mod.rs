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
use crate::settings::ScreensaverSettings;

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
    /// Per-display screensaver config — registered by the daemon at
    /// sink-build time (Task 13).  `None` means no screensaver config
    /// is associated with this display; a `show(RenderScreensaver)` then
    /// resolves with `E_RENDER_UNAVAILABLE` and the engine falls through.
    screensaver: std::sync::Arc<std::sync::Mutex<Option<ScreensaverSettings>>>,
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
            screensaver: std::sync::Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Register the per-display screensaver config so a subsequent
    /// `show(RenderScreensaver)` can build the player.  Replaces any
    /// previously-registered config (T11: the daemon calls this once
    /// at sink-build time; later live config reloads would also call
    /// here).
    ///
    /// Held behind a `Mutex` so a clone of the sink can be live in the
    /// daemon while the config is updated; the wayland thread's `show`
    /// read sees either the old or the new value, never a torn one.
    pub fn set_screensaver(&self, settings: ScreensaverSettings) {
        if let Ok(mut guard) = self.screensaver.lock() {
            *guard = Some(settings);
        }
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
        match kind {
            StageKind::RenderBlack | StageKind::Controller(_) => {
                // Controller stages never reach a render sink at all (the
                // engine routes them through the command-sink chain).
                // If one does, fall through with E_RENDER_UNAVAILABLE.
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
            }
            StageKind::RenderScreensaver => {
                // Without a registered config, fall through to the next
                // ladder stage — same shape as the `RenderBlack` path
                // failing on a missing sink.  The sink's lifetime is the
                // sink's; the daemon's T13 wires `set_screensaver`.
                let settings = self.screensaver.lock().ok().and_then(|guard| guard.clone());
                let Some(settings) = settings else {
                    let _ = reply_tx.send(Err(CmdFailure {
                        controller: "render-screensaver".into(),
                        error: format!(
                            "{E_RENDER_UNAVAILABLE}: no screensaver config registered for display"
                        ),
                    }));
                    return reply_rx
                        .await
                        .map_err(|_recv_err| CmdFailure {
                            controller: "render-screensaver".into(),
                            error: format!("{E_RENDER_UNAVAILABLE}: wayland thread dropped reply"),
                        })
                        .and_then(|inner| inner);
                };
                self.cmd_tx
                    .send(RenderCommand::ShowScreensaver {
                        r#gen,
                        idx,
                        settings,
                        reply: reply_tx,
                    })
                    .map_err(|_send_err| CmdFailure {
                        controller: "render-screensaver".into(),
                        error: format!("{E_RENDER_UNAVAILABLE}: wayland thread not running"),
                    })?;
            }
        }
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
