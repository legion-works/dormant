//! Web server shared state — all the live daemon handles a route needs.
//!
//! [`WebState`] wraps [`WebStateInner`] in an [`Arc`] so the struct is
//! cheaply cloneable even though [`broadcast::Receiver`] is not `Clone`.
//! Every field of [`WebStateInner`] is a `dormant-core`- or
//! `dormant-doctor`-owned type — no `dormantd`-local type, so there is
//! no dependency cycle.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use dormant_core::config::schema::{Config, Credentials};
use dormant_core::error::DormantError;
use dormant_core::reload::ReloadOutcome;
use dormant_core::reload::ReloadRequester;
use dormant_core::rules::{ControlMsg, DaemonEvent};
use dormant_core::types::DisplayId;
use dormant_core::wear::WearHandle;
use dormant_displays::samsung_tizen::{PairConnect, RealPairConnect};
use dormant_doctor::DoctorService;
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::error::WebError;
use crate::event_ring::EventRing;
use crate::routes::pair::{PairEntry, PairId};

/// Seam type for persisting a granted pairing token — factored into a type
/// alias (`clippy::type_complexity`) rather than spelling the trait-object
/// `Fn` type out at every field/parameter that needs it.
pub(crate) type UpsertToken =
    Arc<dyn Fn(&Path, &str, &str) -> Result<(), DormantError> + Send + Sync>;

/// Injectable transport for daemon IPC, keeping HTTP route tests off real sockets.
pub(crate) trait DaemonIpc: Send + Sync {
    /// Send one IPC request and return its decoded response.
    fn request<'a>(
        &'a self,
        socket: PathBuf,
        request: dormant_core::ipc_proto::IpcRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<dormant_core::ipc_proto::IpcResponse, WebError>> + Send + 'a,
        >,
    >;
}

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
///
/// Construct via [`WebStateInner::new`] (production) or
/// `WebStateInner::new_for_test` / `WebStateInner::new_for_test_with_pairing`
/// (`#[cfg(test)]`-only, hence plain code spans rather than doc-links here)
/// — never a bare struct literal; several fields
/// (`apply_lock`, `pairing`, `pair_lock`, `pair_connect`, `upsert_token`,
/// `emergency_wake_lock`) are either always freshly constructed or given a
/// constructor-specific default that a hand-written literal would have to
/// duplicate at every call site.
pub struct WebStateInner {
    /// Engine control channel — used by routes that need a live snapshot
    /// (`/api/state`) or a control action (`/api/blank`, etc.).
    pub ctl_tx: mpsc::Sender<ControlMsg>,

    /// Submit causally-correlated requests to the daemon's reload coordinator.
    pub reload_requester: ReloadRequester,

    /// Subscribe to reload outcomes for the events WS re-subscribe dance.
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
    pub wear: WearHandle,
    /// Redacted daemon-owned active-sampling lifecycle status.
    pub wear_sampling_rx: watch::Receiver<Option<dormant_core::wear::WearSamplingStatus>>,
    /// Per-display redacted sampler statuses (issue #185 cycle B).
    /// The daemon's sampler registry writes one entry per configured
    /// display; `/api/wear` joins on `ledger.identity.config_display_id` to
    /// attribute the right gate + uniform reason to each display. The
    /// legacy singular [`wear_sampling_rx`](Self::wear_sampling_rx) is the
    /// fallback ONLY when this map is empty — never when a display is
    /// merely absent from a populated map. Off-Linux / no-sampler builds
    /// carry an empty map (a fresh `watch::channel(BTreeMap::new())`), so
    /// the route degrades to the legacy singular view there.
    pub per_display_statuses_rx:
        watch::Receiver<BTreeMap<String, dormant_core::wear::WearSamplingStatus>>,

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

    /// Transport used by routes that proxy daemon IPC.
    pub(crate) ipc: Arc<dyn DaemonIpc>,

    /// In-flight and recently-finished Samsung pairing attempts, keyed by
    /// the opaque [`PairId`] handed back from `POST /api/pair/samsung`.
    /// `GET /api/pair/samsung/{id}` lazily sweeps terminal (non-`"pairing"`)
    /// entries older than 5 minutes on every poll — see
    /// [`crate::routes::pair::sweep_expired`]. Values are
    /// [`crate::routes::pair::PairStatus`], which is token-free by
    /// construction — a token never lives in this map.
    pub(crate) pairing: Mutex<HashMap<PairId, PairEntry>>,

    /// Single-flight guard for the pairing wizard: `POST /api/pair/samsung`
    /// takes this via `try_lock_owned` (never `.await`s it) so a second
    /// concurrent pairing attempt gets an immediate 409 instead of queueing
    /// behind the first. Wrapped in its own `Arc` — distinct from the
    /// `Arc<WebStateInner>` that [`WebState`] wraps — because
    /// `try_lock_owned` needs `self: Arc<Mutex<()>>` to hand back an
    /// `OwnedMutexGuard` that can be moved into the `tokio::spawn`ed
    /// pairing task and held for its whole (bounded-by-`pair_timeout`)
    /// duration.
    pub(crate) pair_lock: Arc<Mutex<()>>,

    /// Web-scoped single-flight guard for the active-sampling consent flow.
    pub(crate) wear_sampling_lock: Arc<Mutex<()>>,

    /// Injectable seam for the pairing wizard's TV-connect step —
    /// production wiring is [`RealPairConnect`]; pairing tests inject
    /// `dormant_displays::test_support::FakePairConnect` (feature
    /// `test-util`) via `WebStateInner::new_for_test_with_pairing`.
    pub(crate) pair_connect: Arc<dyn PairConnect>,

    /// Injectable seam for persisting a granted pairing token —
    /// production wiring delegates to
    /// [`dormant_core::config::upsert_samsung_token`] unmodified; tests
    /// substitute a closure that records `(path, host, token)` calls
    /// instead of touching the filesystem.
    pub(crate) upsert_token: UpsertToken,

    /// Web-scoped single-flight guard for global emergency wake.
    ///
    /// The owned guard moves into the reply-monitor task, so an HTTP report
    /// timeout does not admit a second web request while the engine
    /// operation continues.
    pub(crate) emergency_wake_lock: Arc<Mutex<()>>,

    /// Displays with a web control-path exercise still awaiting an engine reply.
    ///
    /// Entries are removed by the detached reply monitor, not by the HTTP timeout.
    pub(crate) exercise_in_flight: Arc<Mutex<HashSet<DisplayId>>>,

    /// Epoch seconds captured once, at `WebState` construction — `GET
    /// /api/daemon`'s proxy for daemon start time. Captured here (rather
    /// than read fresh per request) because the web server is spawned
    /// during `dormantd::app::App::start`, so construction time already
    /// tracks daemon start closely enough for a sidebar uptime display.
    pub(crate) started_epoch_s: u64,

    /// Bounded in-memory ring of recent [`DaemonEvent`]s for
    /// `GET /api/events/recent`.  Fed by a background subscriber task
    /// spawned in [`WebStateInner::assemble`].
    pub(crate) event_history: Arc<EventRing>,

    /// Path to the `star-nudge-dismissed` flag file in the config directory.
    /// Existence of this file means the user has dismissed the "Star the repo"
    /// nudge in the web UI. Read by `GET /api/daemon`; written atomically by
    /// `POST /api/star-nudge/dismiss`.
    pub(crate) star_nudge_path: PathBuf,

    /// Path to the `wear-sampling-nudge-dismissed` flag file in the config
    /// directory. Existence of this file means the user has dismissed the
    /// active-sampling onboarding nudge (#186) on the wear card. Read by
    /// `GET /api/daemon`; written atomically by
    /// `POST /api/wear/sampling/nudge/dismiss`. Mirrors [`star_nudge_path`]
    /// — same persistence discipline (tempfile + rename, `create_new(true)`,
    /// symlink-safe), same config-dir placement.
    pub(crate) wear_sampling_nudge_path: PathBuf,

    /// Test-only seam: explicit path to the `gh` binary for
    /// `POST /api/star-nudge/star`.  When `None` (production), the handler
    /// does a PATH lookup on a fixed, hardened PATH.  Tests inject a
    /// nonexistent path to verify the false-branch without mutating global
    /// env.
    pub(crate) star_gh_path: Option<PathBuf>,

    /// Test-only seam: override [`GH_CHILD_PATH`] with a test directory
    /// containing a stub `gh` binary — no global `env::set_var` needed.
    pub(crate) star_test_path: Option<PathBuf>,
}

/// The subset of [`WebStateInner`]'s fields that vary across construction
/// call sites. Everything else — `apply_lock`, `pairing`, `pair_lock`,
/// `pair_connect`, `upsert_token` — is either always freshly constructed or
/// given a constructor-specific default (see [`WebStateInner::new`] /
/// `WebStateInner::new_for_test`), so grouping just the call-site-varying
/// fields here keeps every construction site's diff small regardless of
/// how many seam fields `WebStateInner` grows over time.
pub struct WebStateInnerParams {
    pub ctl_tx: mpsc::Sender<ControlMsg>,
    pub reload_requester: ReloadRequester,
    pub reload_rx: broadcast::Receiver<ReloadOutcome>,
    pub config_rx: watch::Receiver<Arc<Config>>,
    pub creds_rx: watch::Receiver<Arc<Credentials>>,
    pub config_path: PathBuf,
    pub creds_path: PathBuf,
    pub doctor: DoctorService,
    pub wear: WearHandle,
    pub wear_sampling_rx: watch::Receiver<Option<dormant_core::wear::WearSamplingStatus>>,
    /// Per-display sampler status watch — see
    /// [`WebStateInner::per_display_statuses_rx`]. Off-Linux / no-sampler
    /// call sites pass a fresh `watch::channel(BTreeMap::new()).1`.
    pub per_display_statuses_rx:
        watch::Receiver<BTreeMap<String, dormant_core::wear::WearSamplingStatus>>,
    pub web_bind: SocketAddr,
    pub cancel: CancellationToken,
    pub reload_timeout: Duration,
}

impl WebStateInner {
    /// Production constructor. Defaults the pairing seams to the real
    /// implementations: [`RealPairConnect`] for the TV-connect step and
    /// [`dormant_core::config::upsert_samsung_token`] (unmodified — only
    /// wrapped in a closure to match the `Arc<dyn Fn(..)>` seam shape) for
    /// token persistence.
    #[must_use]
    pub fn new(params: WebStateInnerParams) -> Self {
        Self::assemble(
            params,
            Arc::new(RealPairConnect),
            Arc::new(|path: &Path, host: &str, token: &str| {
                dormant_core::config::upsert_samsung_token(path, host, token)
            }),
            Arc::new(crate::SocketDaemonIpc),
            true,
        )
    }

    /// Snapshot the current web single-flight guards and publish an
    /// authoritative [`DaemonEvent::OperationsChanged`] to the engine event
    /// bus. Called from every guard mutation site (insert, remove — including
    /// the detached completion monitor that outlives an HTTP timeout). The
    /// wire `exercise_in_flight` is sorted ascending so a consumer can diff
    /// successive frames without re-sorting.
    ///
    /// The publish goes through the same `ControlMsg::PublishDaemonEvent`
    /// path the wear tracker and other daemon-lifetime components use, so
    /// it joins the existing broadcast (`Subscribed`-gated) and the
    /// `event_ring` feeder without an extra wiring seam.
    pub(crate) async fn publish_operations_changed(&self) {
        let mut exercise_in_flight: Vec<String> = self
            .exercise_in_flight
            .lock()
            .await
            .iter()
            .map(|display| display.0.clone())
            .collect();
        exercise_in_flight.sort();
        let emergency_wake_in_flight = self.emergency_wake_lock.try_lock().is_err();
        let _ = self
            .ctl_tx
            .send(ControlMsg::PublishDaemonEvent(
                DaemonEvent::OperationsChanged {
                    exercise_in_flight,
                    emergency_wake_in_flight,
                },
            ))
            .await;
    }

    /// Test constructor for call sites that never reach the pairing
    /// wizard (the overwhelming majority of this crate's tests).
    /// `pair_connect` defaults to a PANIC-on-call fake — deliberately NOT
    /// [`RealPairConnect`] — so an accidental reach of the pairing path
    /// fails the test loudly (a panic) instead of silently attempting live
    /// TLS to a TV that doesn't exist in a test environment. `upsert_token`
    /// likewise panics if called. Tests that DO exercise the pairing
    /// wizard must use [`WebStateInner::new_for_test_with_pairing`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new_for_test(params: WebStateInnerParams) -> Self {
        Self::assemble(
            params,
            Arc::new(PanicPairConnect),
            Arc::new(|_: &Path, _: &str, _: &str| -> Result<(), DormantError> {
                unimplemented!(
                    "upsert_token was called on a WebStateInner built via new_for_test — \
                     use new_for_test_with_pairing to inject a fake for pairing-wizard tests"
                )
            }),
            Arc::new(crate::SocketDaemonIpc),
            false,
        )
    }

    /// Test constructor for the pairing-wizard's own tests — injects both
    /// seams explicitly (typically a
    /// `dormant_displays::test_support::FakePairConnect` and a closure that
    /// records `upsert_token` calls into a test-owned `Vec`/`Mutex`).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new_for_test_with_pairing(
        params: WebStateInnerParams,
        pair_connect: Arc<dyn PairConnect>,
        upsert_token: UpsertToken,
    ) -> Self {
        Self::assemble(
            params,
            pair_connect,
            upsert_token,
            Arc::new(crate::SocketDaemonIpc),
            false,
        )
    }

    /// Test constructor with an injected daemon IPC transport.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new_for_test_with_ipc(
        params: WebStateInnerParams,
        ipc: Arc<dyn DaemonIpc>,
    ) -> Self {
        Self::assemble(
            params,
            Arc::new(PanicPairConnect),
            Arc::new(|_: &Path, _: &str, _: &str| -> Result<(), DormantError> {
                unimplemented!("pairing seam reached in IPC route test")
            }),
            ipc,
            false,
        )
    }

    /// Shared assembly — every constructor bottoms out here so the
    /// always-fresh fields (`apply_lock`, `pairing`, `pair_lock`,
    /// `emergency_wake_lock`) are built exactly once, in exactly one place.
    fn assemble(
        params: WebStateInnerParams,
        pair_connect: Arc<dyn PairConnect>,
        upsert_token: UpsertToken,
        ipc: Arc<dyn DaemonIpc>,
        spawn_feeder: bool,
    ) -> Self {
        let event_history = Arc::new(EventRing::new());
        if spawn_feeder {
            let history = event_history.clone();
            let ctl_tx = params.ctl_tx.clone();
            tokio::spawn(async move {
                crate::event_ring::feed_ring(history, ctl_tx).await;
            });
        }

        // CORR 3: if parent is empty (e.g. relative `config.toml`), use
        // the current directory so star_nudge_path is always a real
        // filesystem location rather than panicking.
        let config_dir = params
            .config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let star_nudge_path = config_dir.join("star-nudge-dismissed");
        // #186: companion flag for the active-sampling onboarding nudge.
        // Same config-dir layout as the star nudge so the operator sees
        // one cluster of UI-state files together.
        let wear_sampling_nudge_path = config_dir.join("wear-sampling-nudge-dismissed");

        Self {
            ctl_tx: params.ctl_tx,
            reload_requester: params.reload_requester,
            reload_rx: params.reload_rx,
            config_rx: params.config_rx,
            creds_rx: params.creds_rx,
            config_path: params.config_path,
            creds_path: params.creds_path,
            apply_lock: Mutex::new(()),
            doctor: params.doctor,
            wear: params.wear,
            wear_sampling_rx: params.wear_sampling_rx,
            per_display_statuses_rx: params.per_display_statuses_rx,
            web_bind: params.web_bind,
            cancel: params.cancel,
            reload_timeout: params.reload_timeout,
            ipc,
            pairing: Mutex::new(HashMap::new()),
            pair_lock: Arc::new(Mutex::new(())),
            wear_sampling_lock: Arc::new(Mutex::new(())),
            pair_connect,
            upsert_token,
            emergency_wake_lock: Arc::new(Mutex::new(())),
            exercise_in_flight: Arc::new(Mutex::new(HashSet::new())),
            started_epoch_s: now_epoch_s(),
            event_history,
            star_nudge_path,
            wear_sampling_nudge_path,
            star_gh_path: None,
            star_test_path: None,
        }
    }
}

/// Current wall-clock time as epoch seconds; `0` if the clock is somehow
/// before the epoch (never in practice — defensive only). Mirrors
/// `routes::wear::now_epoch_s` / `dormantd::wear_tracker::now_epoch_s`.
fn now_epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Test-only [`PairConnect`] whose `connect` panics unconditionally.
/// [`WebStateInner::new_for_test`] wires this in as the DEFAULT so any test
/// that accidentally reaches the pairing path fails loudly instead of the
/// alternative — quietly falling back to [`RealPairConnect`] and attempting
/// live TLS to a TV that doesn't exist in a test environment.
#[cfg(test)]
struct PanicPairConnect;

#[cfg(test)]
#[async_trait::async_trait]
impl PairConnect for PanicPairConnect {
    async fn connect(&self, _host: &str, _timeout: Duration) -> Result<String, DormantError> {
        panic!(
            "PanicPairConnect::connect was called — this WebStateInner was built via \
             new_for_test, which defaults pair_connect to a panic-on-call fake so an \
             accidental reach of the pairing path fails loudly instead of attempting \
             live TLS. Use new_for_test_with_pairing to inject a FakePairConnect."
        );
    }
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
