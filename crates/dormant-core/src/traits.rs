//! I/O boundary traits: sensor sources feed [`crate::types::PresenceEvent`]s in,
//! display controllers and command sinks take blank/wake commands out.
//!
//! Everything in this module is a trait — implementations live in
//! `dormant-sensors` and `dormant-displays`; this crate only defines the
//! contract so that [`crate::rules::RulesEngine`] stays free of concrete I/O.

use std::any::Any;

use crate::error::DormantError;
use crate::types::{BlankMode, CmdFailure, PresenceEvent, StageKind};

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
///
/// `Any` is a supertrait so trait objects can be downcast to the concrete
/// controller type in tests (registry construction-path assertions) without
/// exposing test-only accessor methods on the trait itself. Every
/// `DisplayController` impl is `'static` by construction (concrete types
/// stored in `Box<dyn DisplayController>`), so `Any` is satisfied
/// automatically — no per-impl boilerplate required.
#[async_trait::async_trait]
pub trait DisplayController: Any + Send + Sync {
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

    /// One-shot wake: a single attempt through the controller chain, no retries,
    /// no exponential backoff.  Used by [`crate::rules::ControlMsg::EmergencyWake`]
    /// and the `dormantctl emergency-wake` direct-hardware fallback — the
    /// panic-recovery path that needs a fast, best-effort wake command out of
    /// the door.
    ///
    /// The default implementation delegates to [`Self::wake`] for compatibility
    /// with simple sink implementations (e.g. test doubles that don't model
    /// retries).  Production sinks should override with a bounded variant.
    async fn wake_once(&self) -> Result<(), CmdFailure> {
        self.wake().await
    }

    /// Per-controller health from the LAST blank/wake attempt (never
    /// re-probes).  Empty until the first attempt.
    fn controller_health(&self) -> Vec<crate::rules::ControllerHealth>;
}

/// The narrow interface [`crate::rules::RulesEngine`] uses to show and tear
/// down render surfaces (layer-shell overlays) on a display.
///
/// Mirrors the [`CommandSink`] pattern: trait is defined in core, the real
/// implementation lives externally; the engine only names this trait, never
/// the implementation crate.
#[async_trait::async_trait]
pub trait RenderSink: Send + Sync {
    /// Show a render surface for the given ladder stage.
    ///
    /// `r#gen` is a monotonically increasing generation counter for stale-
    /// detection; `idx` identifies the ladder rung; `kind` carries the stage
    /// type so the backend can choose the right surface (black overlay vs
    /// screensaver).
    async fn show(&self, r#gen: u64, idx: usize, kind: StageKind) -> Result<(), CmdFailure>;

    /// Tear down any active render surface on this display.
    ///
    /// Idempotent: calling `teardown` when no surface is up is a no-op.
    /// Infallible: the method has no failure mode — the engine always
    /// considers the surface gone after this call returns.
    async fn teardown(&self, r#gen: u64);
}
