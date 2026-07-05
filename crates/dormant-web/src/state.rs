//! Web server shared state — all the live daemon handles a route needs.
//!
//! [`WebState`] wraps [`WebStateInner`] in an [`Arc`] so the struct is
//! cheaply cloneable even though [`broadcast::Receiver`] is not `Clone`.
//! Every field of [`WebStateInner`] is a `dormant-core`- or
//! `dormant-doctor`-owned type — no `dormantd`-local type, so there is
//! no dependency cycle.

use std::path::PathBuf;
use std::sync::Arc;

use dormant_core::config::schema::{Config, Credentials};
use dormant_core::reload::ReloadOutcome;
use dormant_core::rules::ControlMsg;
use dormant_doctor::DoctorService;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Shared state for the web server.
///
/// Wraps an [`Arc`]`<`[`WebStateInner`]`>` so the struct is cheaply
/// cloneable even though [`broadcast::Receiver`] is not `Clone`.
/// Construct via [`WebState::new`].
#[derive(Clone)]
pub struct WebState {
    pub(crate) inner: Arc<WebStateInner>,
}

/// The live data a web server route needs.  Every field is a
/// `dormant-core`- or `dormant-doctor`-owned type — no `dormantd`-local
/// type, so there is no dependency cycle.
pub struct WebStateInner {
    /// Engine control channel — used by routes that need a live snapshot
    /// (`/api/state`) or a control action (`/api/blank`, etc.).
    pub ctl_tx: mpsc::Sender<ControlMsg>,

    /// Trigger a config reload (fire-and-forget — sent to the daemon's run
    /// loop, not the engine).
    pub reload_trigger: mpsc::Sender<()>,

    /// Subscribe to reload outcomes (for the events WS re-subscribe dance).
    pub reload_rx: broadcast::Receiver<ReloadOutcome>,

    /// Live config watch (read-only receiver, used by `/api/config`).
    pub config_rx: watch::Receiver<Arc<Config>>,

    /// Live credentials watch (read-only receiver, used by `/api/config`).
    pub creds_rx: watch::Receiver<Arc<Credentials>>,

    /// Path to the daemon's config file (for `/api/config` raw display +
    /// validation re-run).
    pub config_path: PathBuf,

    /// Shared, coalesced [`DoctorService`] — same instance the IPC server
    /// uses.
    pub doctor: DoctorService,

    /// Signalled by the daemon on shutdown; the web listener uses this for
    /// graceful shutdown.
    pub cancel: CancellationToken,
}

impl WebState {
    /// Wrap the given inner state behind an [`Arc`] for cheap cloning.
    #[must_use]
    pub fn new(inner: WebStateInner) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}
