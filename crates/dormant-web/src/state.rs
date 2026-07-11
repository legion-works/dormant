//! Web server shared state — all the live daemon handles a route needs.
//!
//! [`WebState`] wraps [`WebStateInner`] in an [`Arc`] so the struct is
//! cheaply cloneable even though [`broadcast::Receiver`] is not `Clone`.
//! Every field of [`WebStateInner`] is a `dormant-core`- or
//! `dormant-doctor`-owned type — no `dormantd`-local type, so there is
//! no dependency cycle.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dormant_core::config::schema::{Config, Credentials};
use dormant_core::reload::ReloadOutcome;
use dormant_core::rules::ControlMsg;
use dormant_doctor::DoctorService;
use tokio::sync::{Mutex, broadcast, mpsc, watch};
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

    /// Path to the daemon's credentials file.  Used by GET and apply
    /// endpoints to load credentials from the same canonical location
    /// rather than deriving the path from the config file name.
    pub creds_path: PathBuf,

    /// Serialises config-apply operations so concurrent apply requests
    /// cannot race each other.
    pub apply_lock: Mutex<()>,

    /// Shared, coalesced [`DoctorService`] — same instance the IPC server
    /// uses.
    pub doctor: DoctorService,

    /// Shared panel-wear ledger map — same [`dormant_core::wear::WearHandle`]
    /// instance the wear tracker writes to (spec §5).  `/api/wear` reads it
    /// directly; no dormantd-local type, so no dependency cycle.
    pub wear: dormant_core::wear::WearHandle,

    /// The socket address the web server is bound to.  Used by the
    /// security middleware to validate the Host header against the
    /// configured bind address.
    pub web_bind: SocketAddr,

    /// Signalled by the daemon on shutdown; the web listener uses this for
    /// graceful shutdown.
    pub cancel: CancellationToken,

    /// Maximum time to wait for a reload outcome after writing the config
    /// file via `POST /api/config/apply`.  Default is 10 s; tests use a
    /// shorter value.
    pub reload_timeout: Duration,
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
