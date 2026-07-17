//! Non-Linux stub of the [`RenderSink`] — keeps `cargo check --workspace`
//! green on macOS / Windows while the real Wayland backend is gated to
//! `target_os = "linux"`.
//!
//! Behaviour:
//!
//! - [`RenderSink::show`] always fails with `E_RENDER_UNAVAILABLE`, so the
//!   engine falls through and `dormantd` can be cross-compiled / built on
//!   dev hosts without a compositor.
//! - [`RenderSink::teardown`] is a no-op (the contract says it's
//!   infallible and idempotent).
//!
//! The signature mirrors the linux backend so callers can construct a
//! sink unconditionally (`use dormant_render::LayerShellRenderSink`) and
//! dispatch on `cfg(target_os = "linux")` only when they want the real
//! behaviour.

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;

use dormant_core::error::E_RENDER_UNAVAILABLE;
use dormant_core::traits::RenderSink;
use dormant_core::types::{CmdFailure, DisplayId, StageKind};

use crate::settings::{ScreensaverSettings, ShiftSettings};

/// Cross-platform placeholder for the Wayland layer-shell backend.
///
/// Carries the constructor arguments (`display_id`, `output_name`,
/// `input_wake_tx`) so the factory can switch backends uniformly; the
/// stub never reaches a compositor so `input_wake_tx` is held but never
/// used on non-Linux.
#[derive(Debug, Clone)]
pub struct LayerShellRenderSink {
    display_id: DisplayId,
    output_name: String,
    #[allow(dead_code)] // mirror of linux signature; never used on non-Linux.
    input_wake_tx: Option<UnboundedSender<DisplayId>>,
}

impl LayerShellRenderSink {
    /// Construct a stub. Never fails on non-Linux — the absence of a real
    /// backend is reported on the first [`RenderSink::show`] call.
    pub fn new(
        display_id: DisplayId,
        output_name: String,
        input_wake_tx: Option<&UnboundedSender<DisplayId>>,
    ) -> Result<Self, CmdFailure> {
        Ok(Self {
            display_id,
            output_name,
            input_wake_tx: input_wake_tx.cloned(),
        })
    }

    /// Identifier of the display this sink was built for.  Exposed for
    /// logging / diagnostics.
    #[must_use]
    pub fn display_id(&self) -> &DisplayId {
        &self.display_id
    }

    /// Connector name of the output this sink was built for (e.g.
    /// `"DP-1"`).  Exposed for logging / diagnostics.
    #[must_use]
    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    /// Register the per-display screensaver config.  No-op on this
    /// platform — the stub can never show a screensaver overlay.
    /// Kept so callers can invoke it unconditionally.
    pub fn set_screensaver(&self, _settings: ScreensaverSettings) {
        // No compositor → no-op.
    }

    /// Register the per-display pixel-shift config.  No-op on this
    /// platform — mirrors [`Self::set_screensaver`]; kept so callers
    /// (`dormantd::app::build_render_sinks`) can invoke it
    /// unconditionally regardless of target OS.
    pub fn set_shift(&self, _shift: ShiftSettings) {
        // No compositor → no-op.
    }
}

#[async_trait]
impl RenderSink for LayerShellRenderSink {
    async fn show(&self, _gen: u64, _idx: usize, kind: StageKind) -> Result<(), CmdFailure> {
        Err(CmdFailure {
            controller: "render-black".into(),
            error: format!(
                "{E_RENDER_UNAVAILABLE}: render backend unavailable on this platform (stage {kind:?})"
            ),
        })
    }

    async fn teardown(&self, _gen: u64) {
        // Infallible no-op — the contract is explicit.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::types::StageKind;

    #[tokio::test]
    async fn show_render_black_returns_unavailable() {
        let sink =
            LayerShellRenderSink::new(DisplayId("display-A".into()), "DP-1".into(), None).unwrap();
        let result = sink.show(1, 0, StageKind::RenderBlack).await;
        let err = result.expect_err("stub show must error");
        assert_eq!(err.controller, "render-black");
        assert!(
            err.error.starts_with(E_RENDER_UNAVAILABLE),
            "error must start with E_RENDER_UNAVAILABLE, got: {}",
            err.error,
        );
    }

    #[tokio::test]
    async fn show_render_screensaver_returns_unavailable() {
        let sink =
            LayerShellRenderSink::new(DisplayId("display-A".into()), "DP-1".into(), None).unwrap();
        let result = sink.show(1, 0, StageKind::RenderScreensaver).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn show_controller_stage_returns_unavailable() {
        let sink =
            LayerShellRenderSink::new(DisplayId("display-A".into()), "DP-1".into(), None).unwrap();
        let result = sink
            .show(
                1,
                0,
                StageKind::Controller(dormant_core::types::BlankMode::PowerOff),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn teardown_is_infallible_and_noop() {
        let sink =
            LayerShellRenderSink::new(DisplayId("display-A".into()), "DP-1".into(), None).unwrap();
        sink.teardown(99).await;
    }

    #[test]
    fn accessors_return_constructor_args() {
        let sink =
            LayerShellRenderSink::new(DisplayId("display-B".into()), "HDMI-A-1".into(), None)
                .unwrap();
        assert_eq!(sink.display_id().to_string(), "display-B");
        assert_eq!(sink.output_name(), "HDMI-A-1");
    }
}
