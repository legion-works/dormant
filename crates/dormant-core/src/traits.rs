//! I/O boundary traits: sensor sources feed [`crate::types::PresenceEvent`]s in,
//! display controllers and command sinks take blank/wake commands out.
//!
//! Everything in this module is a trait — implementations live in
//! `dormant-sensors` and `dormant-displays`; this crate only defines the
//! contract so that [`crate::rules::RulesEngine`] stays free of concrete I/O.

use crate::error::DormantError;
use crate::types::{BlankMode, CmdFailure, PresenceEvent};

/// A source of presence observations for one or more sensors (MQTT, Home
/// Assistant WebSocket, USB-serial radar, ...).
#[async_trait::async_trait]
pub trait SensorSource: Send {
    /// Stable identifier for this source, used in logs and error messages.
    fn source_id(&self) -> &str;

    /// Runs until `cancel` is triggered, pushing [`PresenceEvent`]s into `tx`.
    ///
    /// On internal failure (broker disconnect, USB unplug, ...) this method
    /// **must** emit [`crate::types::SensorState::Unavailable`] for all of its
    /// sensors before retrying or returning — fail-safe presence depends on
    /// unavailability being reported, never silently dropped.
    async fn run(
        self: Box<Self>,
        tx: tokio::sync::mpsc::Sender<PresenceEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()>;
}

/// A display controller: local (`KWin` DPMS, DDC/CI) or network (Samsung Tizen,
/// LG webOS, HA passthrough, arbitrary command).
#[async_trait::async_trait]
pub trait DisplayController: Send + Sync {
    /// Literal name of this controller (grep-stable, matches config `type`).
    fn name(&self) -> &'static str;

    /// Blank modes this controller can execute.
    fn supported_modes(&self) -> Vec<BlankMode>;

    /// One-time startup probe (capability detection, reachability check).
    ///
    /// Default implementation does nothing and always succeeds.
    async fn probe(&mut self) -> Result<(), DormantError> {
        Ok(())
    }

    /// Whether the controller currently believes the display is reachable.
    async fn is_available(&self) -> bool;

    /// Blank the display using the given mode.
    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure>;

    /// Wake the display.
    async fn wake(&self) -> Result<(), CmdFailure>;
}

/// The narrow interface [`crate::rules::RulesEngine`] uses to issue commands to
/// a display, hiding controller selection, fallback, and retries behind one
/// per-display object composed by the daemon.
#[async_trait::async_trait]
pub trait CommandSink: Send + Sync {
    /// Blank the display using the given mode.
    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure>;

    /// Wake the display.
    async fn wake(&self) -> Result<(), CmdFailure>;
}
