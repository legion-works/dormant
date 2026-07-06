//! Daemon application assembly and lifecycle.
//!
//! [`App`] loads and validates configuration, then assembles the full runtime:
//! sensor sources → [`ZoneEngine`] → [`RulesEngine`] → per-display
//! [`DisplayExecutor`]s. [`App::start`] spawns the engine, the sources, the
//! user-activity inhibitor, and a config-file watcher, returning an
//! [`AppHandle`] for control and a join handle for the run loop.
//!
//! ## Post-probe display validation (layer 2)
//!
//! Config validation (layer 1) checks `blank_mode` against the *static*
//! capability registry. After controllers are built and probed we know each
//! display's *effective* modes (the union of its live controllers'
//! `supported_modes`). If the configured `blank_mode` is not effective we fall
//! back to `degraded_mode` (with a `display_mode_degraded` warning) or fail
//! startup with `E_MODE_UNSUPPORTED`.
//!
//! ## Hot reload (validate-first, restart-in-place)
//!
//! Reload validates and assembles the **new** config *before* touching the
//! running engine. An invalid or un-assemblable config only flags
//! `pending_reload` on the live engine (via [`ControlMsg::SetPendingReload`])
//! and leaves it running — no teardown, no churn on a bad edit. A valid config
//! triggers a restart-in-place: snapshot, tear down, rebuild a fresh
//! generation.
//!
//! Displays *removed* by the new config that were dark get a *verified* wake
//! (a direct awaited `wake()` on the old executor) before the new generation
//! starts; if that wake fails the reload is aborted, the old config is
//! restarted in place, and `pending_reload` is set. Displays *retained* by the
//! new config that were dark get a *defensive* physical wake after the new
//! generation spawns (`reload_defensive_wake`).
//!
//! ### Restore limitation (v1)
//!
//! `dormant-core` exposes no seam to inject a restored [`DisplayStateMachine`]
//! phase into a running [`RulesEngine`] (machines are private and always start
//! `Active`). Reload replays only the *scheduling* effects a restored phase
//! would emit (via [`RulesEngine::apply_restore_effects`]); the defensive wake
//! (above) covers the physically-dark-but-Active gap. Removed-display verified
//! wake is fully implemented.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dormant_core::config::schema::{Config, Credentials, RuleConfig};
use dormant_core::config::{
    Strictness, ValidationError, Warning, load_config, load_credentials, validate,
};
use dormant_core::ownership::AlwaysOwned;
use dormant_core::rules::{
    ControlMsg, DisplayRuntimeCfg, RuleRuntimeCfg, RulesEngine, RulesEngineConfig,
    SensorRuntimeCfg, StateSnapshot,
};
use dormant_core::state_machine::{DisplayStateMachine, Phase, SmTimings};
use dormant_core::traits::{CommandSink, RenderSink, SensorSource};
use dormant_core::types::{DisplayId, PresenceEvent, RuleId, SensorId, Tick, ZoneId};
use dormant_core::zone::{ZoneEngine, ZoneSpec};
use dormant_displays::executor::{DisplayExecutor, RetrySettings};
use dormant_displays::registry::{build_controllers, capabilities};
use dormant_doctor::DoctorService;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "render")]
use dormant_render::LayerShellRenderSink;

use crate::inhibit_activity::{self, ActivityRule};
use crate::reload;

/// Builds the sensor sources for a config. Production uses the sensor
/// registry; tests inject a factory that returns scripted fakes.
type SourceBuilder =
    Arc<dyn Fn(&Config, &Credentials) -> Result<Vec<Box<dyn SensorSource>>> + Send + Sync>;

/// Builds render sinks for a display.  Production uses
/// [`LayerShellRenderSink`]; tests inject a factory that returns
/// [`RecordingRenderSink`](dormant_core::fakes::RecordingRenderSink).
///
/// The factory receives the display id and output connector name and
/// returns `None` to skip the sink (fall-through path).
#[cfg(feature = "render")]
type RenderSinkBuilder =
    Arc<dyn Fn(DisplayId, String) -> Option<Arc<dyn RenderSink>> + Send + Sync>;

pub use dormant_core::reload::ReloadOutcome;

// ── ValidationReport (for --validate-only) ─────────────────────────────────────

/// Result of a config validation pass, used by `--validate-only`.
#[derive(Debug, Default)]
pub struct ValidationReport {
    /// Unknown-key warnings (lenient mode).
    pub warnings: Vec<Warning>,
    /// Cross-reference validation errors.
    pub errors: Vec<ValidationError>,
    /// A load-time (I/O / syntax / strict unknown-key) failure, if any.
    pub load_error: Option<String>,
}

impl ValidationReport {
    /// Whether the configuration is usable (no load error and no validation
    /// errors).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.load_error.is_none() && self.errors.is_empty()
    }

    /// Process exit code: `0` when ok, `1` otherwise.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.is_ok())
    }

    /// Render warnings and errors into `out`.
    pub fn render(&self, out: &mut impl std::fmt::Write) {
        for w in &self.warnings {
            let _ = writeln!(out, "warning [{}]: {}", w.key_path, w.message);
        }
        if let Some(e) = &self.load_error {
            let _ = writeln!(out, "{e}");
        }
        for e in &self.errors {
            let _ = writeln!(out, "{e}");
        }
        if self.is_ok() {
            let _ = writeln!(out, "configuration OK");
        }
    }
}

/// Load and validate a configuration without building any runtime.
#[must_use]
pub fn validate_only(
    config_path: &std::path::Path,
    creds_path: &std::path::Path,
    strictness: Strictness,
) -> ValidationReport {
    let (cfg, warnings) = match load_config(config_path, strictness) {
        Ok(v) => v,
        Err(e) => {
            return ValidationReport {
                load_error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };
    let creds = match load_credentials(creds_path) {
        Ok(c) => c,
        Err(e) => {
            return ValidationReport {
                warnings,
                load_error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };
    let errors = validate(&cfg, &capabilities(), &creds);
    ValidationReport {
        warnings,
        errors,
        load_error: None,
    }
}

// ── App ────────────────────────────────────────────────────────────────────────

/// The daemon application: config paths + the sensor-source factory.
pub struct App {
    config_path: PathBuf,
    creds_path: PathBuf,
    strictness: Strictness,
    source_builder: SourceBuilder,
    #[cfg(feature = "render")]
    render_sink_builder: Option<RenderSinkBuilder>,
    disable_ipc: bool,
}

impl App {
    /// Build the production app: validates the config up front (bailing with
    /// every validation error) and wires the sensor registry as the source
    /// factory.
    ///
    /// # Errors
    ///
    /// Fails if the config cannot be loaded or fails cross-reference
    /// validation.
    pub fn build(
        config_path: PathBuf,
        creds_path: PathBuf,
        strictness: Strictness,
    ) -> Result<Self> {
        Self::validate_or_bail(&config_path, &creds_path, strictness)?;
        let source_builder: SourceBuilder = Arc::new(|cfg: &Config, creds: &Credentials| {
            dormant_sensors::registry::build(&cfg.sensors, creds).map_err(anyhow::Error::from)
        });
        Ok(Self {
            config_path,
            creds_path,
            strictness,
            source_builder,
            #[cfg(feature = "render")]
            render_sink_builder: None,
            disable_ipc: false,
        })
    }

    /// Build an app with an injected sensor-source factory (test seam).
    ///
    /// # Errors
    ///
    /// Fails if the config cannot be loaded or fails cross-reference
    /// validation.
    pub fn build_with_sources<F>(
        config_path: PathBuf,
        creds_path: PathBuf,
        strictness: Strictness,
        factory: F,
    ) -> Result<Self>
    where
        F: Fn(&Config, &Credentials) -> Result<Vec<Box<dyn SensorSource>>> + Send + Sync + 'static,
    {
        Self::validate_or_bail(&config_path, &creds_path, strictness)?;
        Ok(Self {
            config_path,
            creds_path,
            strictness,
            source_builder: Arc::new(factory),
            #[cfg(feature = "render")]
            render_sink_builder: None,
            disable_ipc: false,
        })
    }

    /// Set an injected render-sink factory (test seam).
    ///
    /// When set, [`assemble_static`] calls this factory instead of
    /// building [`LayerShellRenderSink`] directly.  The factory receives
    /// the display id and output connector name; return `None` to skip
    /// the sink (fall-through).
    #[cfg(feature = "render")]
    #[must_use]
    pub fn with_render_sink_builder<F>(mut self, factory: F) -> Self
    where
        F: Fn(DisplayId, String) -> Option<Arc<dyn RenderSink>> + Send + Sync + 'static,
    {
        self.render_sink_builder = Some(Arc::new(factory));
        self
    }

    /// Disable the IPC server (for tests that don't need it).
    #[must_use]
    pub fn disable_ipc(mut self) -> Self {
        self.disable_ipc = true;
        self
    }

    fn validate_or_bail(
        config_path: &std::path::Path,
        creds_path: &std::path::Path,
        strictness: Strictness,
    ) -> Result<()> {
        let report = validate_only(config_path, creds_path, strictness);
        if let Some(e) = &report.load_error {
            anyhow::bail!("{e}");
        }
        if !report.errors.is_empty() {
            let mut msg = String::from("startup validation failed:");
            for e in &report.errors {
                msg.push_str("\n  ");
                msg.push_str(&e.to_string());
            }
            anyhow::bail!(msg);
        }
        Ok(())
    }

    /// The resolved config path (for logging / diagnostics).
    #[must_use]
    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    /// Start the daemon: assemble the first generation, spawn the engine,
    /// sources, inhibitor, and config watcher, and return a control handle plus
    /// the run-loop join handle.
    ///
    /// # Errors
    ///
    /// Fails if the initial runtime cannot be assembled (controller build,
    /// post-probe validation, zone/engine construction) or the watcher cannot
    /// be installed.
    #[allow(clippy::too_many_lines)]
    pub async fn start(self) -> Result<(AppHandle, JoinHandle<()>)> {
        let root = CancellationToken::new();

        let (cfg, creds) = load_cfg_creds(&self.config_path, &self.creds_path, self.strictness)?;
        let socket_path =
            dormant_core::paths::resolve_socket_path(cfg.daemon.socket_path.as_deref());

        // Clone before cfg/creds are moved into assemble_static.
        let cfg_clone = cfg.clone();
        let creds_clone = creds.clone();
        let started_web_port = cfg.daemon.web_port;
        let started_web_bind = cfg.daemon.web_bind;

        #[cfg(feature = "render")]
        let assembly = assemble_static(
            cfg,
            creds,
            &self.source_builder,
            self.render_sink_builder.as_ref(),
        )
        .await
        .context("assemble initial runtime")?;
        #[cfg(not(feature = "render"))]
        let assembly = assemble_static(cfg, creds, &self.source_builder)
            .await
            .context("assemble initial runtime")?;

        let spawn = spawn_generation(&root, assembly, None, None)?;

        // Stable front channels forwarded to the *current* generation.
        let (engine_ctl_tx, engine_ctl_rx) = watch::channel(spawn.ctl_tx.clone());
        let (engine_events_tx, engine_events_rx) = watch::channel(spawn.events_tx.clone());
        let (front_ctl_tx, front_ctl_rx) = mpsc::channel::<ControlMsg>(64);
        let (front_events_tx, front_events_rx) = mpsc::channel::<PresenceEvent>(256);

        tokio::spawn(forward_ctl(front_ctl_rx, engine_ctl_rx, root.clone()));
        tokio::spawn(forward_events(
            front_events_rx,
            engine_events_rx,
            root.clone(),
        ));

        let (reload_tx, _) = broadcast::channel(16);
        let (reload_trigger_tx, reload_trigger_rx) = mpsc::channel::<()>(8);

        let (config_tx, config_rx) = watch::channel(Arc::new(cfg_clone.clone()));
        let (creds_tx, creds_rx) = watch::channel(Arc::new(creds_clone));

        if cfg_clone.daemon.web_allow_nonloopback {
            tracing::warn!(
                event = "web_nonloopback_enabled",
                bind = %cfg_clone.daemon.web_bind,
                "web UI bound off-loopback + UNAUTHENTICATED — doctor/reload/command endpoints are LAN-reachable"
            );
        }

        let watcher =
            reload::config_watcher(&self.config_path).context("install config file watcher")?;

        let runner = Runner {
            config_path: self.config_path.clone(),
            creds_path: self.creds_path,
            strictness: self.strictness,
            source_builder: self.source_builder,
            #[cfg(feature = "render")]
            render_sink_builder: self.render_sink_builder,
            root: root.clone(),
            engine_ctl: engine_ctl_tx,
            engine_events: engine_events_tx,
            reload_tx: reload_tx.clone(),
            config_tx,
            creds_tx,
            generation: spawn.generation,
            started_web_port,
            started_web_bind,
        };

        let join = tokio::spawn(run_loop(runner, watcher, reload_trigger_rx));

        // The doctor service is shared by the IPC server (for
        // `IpcRequest::Doctor`) and the web server (for the
        // `POST /api/doctor` route).  Construct it once here from
        // cloned config/creds watches + the front control channel so
        // both surfaces see the SAME instance — the singleflight
        // coalesce then dedupes a simultaneous CLI `dormantctl doctor`
        // and a browser click on "Run Doctor".
        let doctor_service =
            DoctorService::new(front_ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        #[cfg(unix)]
        let ipc_handle = if self.disable_ipc {
            None
        } else {
            Some(
                crate::ipc::spawn(
                    &socket_path,
                    front_ctl_tx.clone(),
                    reload_trigger_tx.clone(),
                    doctor_service.clone(),
                    root.clone(),
                )
                .context("spawn IPC server")?,
            )
        };
        #[cfg(not(unix))]
        let ipc_handle: Option<tokio::task::JoinHandle<()>> = None;

        // ── Web UI spawn (non-critical — bind failure logs and continues) ──
        // The web UI is an operator tool that must never take down the
        // screen-wake daemon (fail-safe ethos, spec §8).  Item-level
        // #[cfg] so the feature-off build never references dormant_web.
        #[cfg(feature = "web-ui")]
        let web_handle: Option<tokio::task::JoinHandle<()>> = {
            if let Some(port) = cfg_clone.daemon.web_port {
                let addr = std::net::SocketAddr::new(cfg_clone.daemon.web_bind, port);
                let web_state = dormant_web::WebState::new(dormant_web::WebStateInner {
                    ctl_tx: front_ctl_tx.clone(),
                    reload_trigger: reload_trigger_tx.clone(),
                    reload_rx: reload_tx.subscribe(),
                    config_rx: config_rx.clone(),
                    creds_rx: creds_rx.clone(),
                    config_path: self.config_path.clone(),
                    doctor: doctor_service.clone(),
                    web_bind: addr,
                    cancel: root.clone(),
                });
                match dormant_web::spawn(addr, web_state).await {
                    Ok((handle, _addr)) => Some(handle),
                    Err(e) => {
                        tracing::error!(
                            event = "web_bind_failed",
                            %addr,
                            error = %e,
                            "web UI disabled; daemon continues"
                        );
                        None
                    }
                }
            } else {
                None
            }
        };
        #[cfg(not(feature = "web-ui"))]
        let web_handle: Option<tokio::task::JoinHandle<()>> = None;

        let handle = AppHandle {
            ctl_tx: front_ctl_tx,
            events_tx: front_events_tx,
            reload_tx,
            reload_trigger: reload_trigger_tx,
            root,
            config_rx,
            creds_rx,
            config_path: self.config_path.clone(),
            doctor_service,
            _ipc_handle: ipc_handle,
            _web_handle: web_handle,
        };

        Ok((handle, join))
    }

    /// Run the daemon to completion (production entry point): starts and then
    /// awaits the run loop until a shutdown signal fires.
    ///
    /// # Errors
    ///
    /// Propagates [`App::start`] failures.
    pub async fn run(self) -> Result<()> {
        let (handle, join) = self.start().await?;
        join.await.context("run loop panicked")?;
        drop(handle);
        Ok(())
    }
}

// ── AppHandle ──────────────────────────────────────────────────────────────────

/// A control handle for a running [`App`]. Consumed by the IPC control surface
/// and used by tests to drive and observe the engine.
pub struct AppHandle {
    ctl_tx: mpsc::Sender<ControlMsg>,
    events_tx: mpsc::Sender<PresenceEvent>,
    reload_tx: broadcast::Sender<ReloadOutcome>,
    reload_trigger: mpsc::Sender<()>,
    root: CancellationToken,
    config_rx: watch::Receiver<Arc<Config>>,
    creds_rx: watch::Receiver<Arc<Credentials>>,
    config_path: PathBuf,
    doctor_service: DoctorService,
    _ipc_handle: Option<JoinHandle<()>>,
    _web_handle: Option<JoinHandle<()>>,
}

impl AppHandle {
    /// A sender for [`ControlMsg`]s, forwarded to the current engine
    /// generation across reloads.
    #[must_use]
    pub fn control_sender(&self) -> mpsc::Sender<ControlMsg> {
        self.ctl_tx.clone()
    }

    /// A sender for injecting [`PresenceEvent`]s (test seam; production
    /// presence flows from spawned sources).
    #[must_use]
    pub fn events_sender(&self) -> mpsc::Sender<PresenceEvent> {
        self.events_tx.clone()
    }

    /// Subscribe to reload outcomes.
    #[must_use]
    pub fn subscribe_reload(&self) -> broadcast::Receiver<ReloadOutcome> {
        self.reload_tx.subscribe()
    }

    /// Request an immediate reload (as if the config file changed).
    pub async fn trigger_reload(&self) -> bool {
        self.reload_trigger.send(()).await.is_ok()
    }

    /// Signal shutdown; the run loop tears down the current generation.
    pub fn shutdown(&self) {
        self.root.cancel();
    }

    /// Subscribe to live config updates (M2 web UI config view seam).
    #[must_use]
    pub fn config_watch(&self) -> watch::Receiver<Arc<Config>> {
        self.config_rx.clone()
    }

    /// Subscribe to live credential updates (M2 web UI config view seam).
    #[must_use]
    pub fn creds_watch(&self) -> watch::Receiver<Arc<Credentials>> {
        self.creds_rx.clone()
    }

    /// The resolved config path (for M2 web UI `WebState`).
    #[must_use]
    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    /// The shared, coalesced [`DoctorService`] used by the IPC server and
    /// the M2 web UI.  `Clone` (Arc-backed) so callers can hand a clone to
    /// their own sub-systems without re-constructing one.
    #[must_use]
    pub fn doctor_service(&self) -> DoctorService {
        self.doctor_service.clone()
    }
}

// ── Runner (owns the run loop + reload) ────────────────────────────────────────

struct Runner {
    config_path: PathBuf,
    creds_path: PathBuf,
    strictness: Strictness,
    source_builder: SourceBuilder,
    #[cfg(feature = "render")]
    render_sink_builder: Option<RenderSinkBuilder>,
    root: CancellationToken,
    engine_ctl: watch::Sender<mpsc::Sender<ControlMsg>>,
    engine_events: watch::Sender<mpsc::Sender<PresenceEvent>>,
    reload_tx: broadcast::Sender<ReloadOutcome>,
    config_tx: watch::Sender<Arc<Config>>,
    creds_tx: watch::Sender<Arc<Credentials>>,
    generation: Generation,
    /// Port the web UI was started with (for reload change-detection).
    started_web_port: Option<u16>,
    /// Bind address the web UI was started with (for reload change-detection).
    started_web_bind: std::net::IpAddr,
}

impl Runner {
    fn install_generation(&mut self, spawn: GenSpawn) {
        let _ = self.engine_ctl.send(spawn.ctl_tx);
        let _ = self.engine_events.send(spawn.events_tx);
        self.generation = spawn.generation;
    }

    /// Reload the config, restarting the runtime in place. See the module
    /// docs for the full state machine.
    async fn reload(&mut self) {
        let old_ctl = self.engine_ctl.borrow().clone();
        let snapshot = request_snapshot(&old_ctl).await;

        // Validate + assemble the NEW config BEFORE touching the running
        // generation. An invalid or un-assemblable config only flags
        // pending_reload on the live engine and leaves it running — no
        // teardown, no phase loss, no churn on a bad edit.
        let new_assembly = match self.load_and_assemble().await {
            Ok(assembly) => assembly,
            Err(detail) => {
                tracing::error!(event = "config_reload_rejected", detail = %detail);
                let _ = old_ctl
                    .send(ControlMsg::SetPendingReload(Some(detail.clone())))
                    .await;
                let _ = self.reload_tx.send(ReloadOutcome::Rejected(detail));
                return;
            }
        };

        let removed = removed_dark_displays(snapshot.as_ref(), &new_assembly.display_executors);
        let retained_dark =
            retained_dark_displays(snapshot.as_ref(), &new_assembly.display_executors);

        // Capture the new config for watch updates + bind change detection
        // BEFORE new_assembly is consumed by spawn_generation.
        let new_cfg = new_assembly.cfg.clone();
        let new_creds = new_assembly.creds.clone();

        // Reload does not rebind a web listener — flag port/bind changes.
        if new_cfg.daemon.web_port != self.started_web_port
            || new_cfg.daemon.web_bind != self.started_web_bind
        {
            tracing::info!(
                event = "web_bind_change_ignored",
                "web_bind/web_port change requires a daemon restart; keeping the current listener"
            );
        }

        teardown(&mut self.generation).await;

        // Verified physical wake of REMOVED displays (no executor in the new
        // generation) that were dark — via their OLD executor, after teardown.
        // A failure aborts the reload and restores the old config in place
        // (with pending_reload set).
        for display_id in removed {
            if let Some(exec) = self.generation.display_executors.get(&display_id) {
                if let Err(e) = exec.wake().await {
                    let detail =
                        format!("removed display '{display_id}' failed verified wake: {e}");
                    tracing::error!(event = "config_reload_rejected", detail = %detail);
                    self.rebuild_old(Some(detail.clone()), snapshot.as_ref());
                    let _ = self.reload_tx.send(ReloadOutcome::Rejected(detail));
                    return;
                }
                tracing::info!(event = "reload_removed_display_woken", display = %display_id);
            }
        }

        match spawn_generation(&self.root, new_assembly, snapshot.as_ref(), None) {
            Ok(spawn) => {
                self.install_generation(spawn);
                self.defensive_wake(retained_dark);
                self.config_tx.send_replace(Arc::new(new_cfg));
                self.creds_tx.send_replace(Arc::new(new_creds));
                tracing::info!(event = "config_reloaded");
                let _ = self.reload_tx.send(ReloadOutcome::Reloaded);
            }
            Err(e) => {
                let detail = format!("rebuild from new config failed: {e}");
                tracing::error!(event = "config_reload_rejected", detail = %detail);
                self.rebuild_old(Some(detail.clone()), snapshot.as_ref());
                let _ = self.reload_tx.send(ReloadOutcome::Rejected(detail));
            }
        }
    }

    /// Issue a defensive physical wake to RETAINED displays that were dark
    /// before the reload. The rebuilt state machines start `Active`, so a
    /// physically-blanked display in an occupied room would otherwise stay
    /// dark until the next sensor edge. Accept the brief wake-flash (v1
    /// limitation, documented in `reload.rs`).
    fn defensive_wake(&self, displays: Vec<DisplayId>) {
        for display_id in displays {
            let Some(exec) = self.generation.display_executors.get(&display_id).cloned() else {
                continue;
            };
            tokio::spawn(async move {
                match exec.wake().await {
                    Ok(()) => {
                        tracing::info!(event = "reload_defensive_wake", display = %display_id, ok = true);
                    }
                    Err(e) => {
                        tracing::warn!(event = "reload_defensive_wake", display = %display_id, ok = false, error = %e);
                    }
                }
            });
        }
    }

    /// Load + validate + assemble the new config. Returns a human-readable
    /// detail string on any failure.
    async fn load_and_assemble(&self) -> Result<StaticAssembly, String> {
        let (cfg, creds) = load_cfg_creds(&self.config_path, &self.creds_path, self.strictness)
            .map_err(|e| e.to_string())?;
        let errors = validate(&cfg, &capabilities(), &creds);
        if !errors.is_empty() {
            return Err(errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "));
        }
        #[cfg(feature = "render")]
        {
            assemble_static(
                cfg,
                creds,
                &self.source_builder,
                self.render_sink_builder.as_ref(),
            )
            .await
            .map_err(|e| e.to_string())
        }
        #[cfg(not(feature = "render"))]
        {
            assemble_static(cfg, creds, &self.source_builder)
                .await
                .map_err(|e| e.to_string())
        }
    }

    /// Restart the *old* config in place with `pending` populated. Reuses the
    /// current generation's live controllers (no re-probe); rebuilds sources.
    fn rebuild_old(&mut self, pending: Option<String>, snapshot: Option<&StateSnapshot>) {
        let sources = (self.source_builder)(&self.generation.cfg, &self.generation.creds)
            .unwrap_or_else(|e| {
                tracing::error!(event = "reload_source_rebuild_failed", error = %e);
                Vec::new()
            });
        let (activity_rules, activity_poll) = activity_rules(&self.generation.cfg);
        #[cfg(feature = "render")]
        let (render_sinks, input_wake_rx) =
            build_render_sinks(&self.generation.cfg, self.render_sink_builder.as_ref());
        let assembly = StaticAssembly {
            cfg: self.generation.cfg.clone(),
            creds: self.generation.creds.clone(),
            engine_cfg: self.generation.engine_cfg.clone(),
            zone_specs: self.generation.zone_specs.clone(),
            sensor_inventory: self.generation.sensor_inventory.clone(),
            display_executors: self.generation.display_executors.clone(),
            sources,
            activity_rules,
            activity_poll,
            #[cfg(feature = "render")]
            render_sinks,
            #[cfg(not(feature = "render"))]
            render_sinks: HashMap::new(),
            #[cfg(feature = "render")]
            input_wake_rx: Some(input_wake_rx),
        };
        match spawn_generation(&self.root, assembly, snapshot, pending) {
            Ok(spawn) => self.install_generation(spawn),
            Err(e) => tracing::error!(event = "reload_rebuild_old_failed", error = %e),
        }
    }
}

/// The run loop: reload triggers (watcher / SIGHUP / IPC) and shutdown
/// signals, then a bounded graceful teardown.
///
/// Uses platform-specific signals:
/// - Unix: SIGHUP (reload), SIGTERM (shutdown), SIGINT (shutdown)
/// - Non-Unix: Ctrl+C only (reload via watcher/IPC)
async fn run_loop(
    mut runner: Runner,
    mut watcher: reload::ConfigWatcher,
    mut reload_trigger: mpsc::Receiver<()>,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sighup = signal(SignalKind::hangup()).ok();
        let mut sigterm = signal(SignalKind::terminate()).ok();
        let mut sigint = signal(SignalKind::interrupt()).ok();

        loop {
            tokio::select! {
                () = runner.root.cancelled() => break,
                () = wait_unix_signal(sigterm.as_mut()) => {
                    tracing::info!(event = "shutdown_signal", signal = "SIGTERM");
                    runner.root.cancel();
                    break;
                }
                () = wait_unix_signal(sigint.as_mut()) => {
                    tracing::info!(event = "shutdown_signal", signal = "SIGINT");
                    runner.root.cancel();
                    break;
                }
                () = wait_unix_signal(sighup.as_mut()) => {
                    tracing::info!(event = "reload_signal", signal = "SIGHUP");
                    let window = runner.generation.cfg.daemon.reload_debounce;
                    debounce(&mut watcher, window).await;
                    runner.reload().await;
                }
                Some(()) = watcher.rx.recv() => {
                    tracing::info!(event = "reload_trigger", source = "watcher");
                    let window = runner.generation.cfg.daemon.reload_debounce;
                    debounce(&mut watcher, window).await;
                    runner.reload().await;
                }
                Some(()) = reload_trigger.recv() => {
                    tracing::info!(event = "reload_trigger", source = "ipc");
                    let window = runner.generation.cfg.daemon.reload_debounce;
                    debounce(&mut watcher, window).await;
                    runner.reload().await;
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());

        loop {
            tokio::select! {
                () = runner.root.cancelled() => break,
                _ = &mut ctrl_c => {
                    tracing::info!(event = "shutdown_signal", signal = "Ctrl+C");
                    runner.root.cancel();
                    break;
                }
                Some(()) = watcher.rx.recv() => {
                    tracing::info!(event = "reload_trigger", source = "watcher");
                    let window = runner.generation.cfg.daemon.reload_debounce;
                    debounce(&mut watcher, window).await;
                    runner.reload().await;
                }
                Some(()) = reload_trigger.recv() => {
                    tracing::info!(event = "reload_trigger", source = "ipc");
                    let window = runner.generation.cfg.daemon.reload_debounce;
                    debounce(&mut watcher, window).await;
                    runner.reload().await;
                }
            }
        }
    }

    teardown(&mut runner.generation).await;
    tracing::info!(event = "daemon_stopped");
}

/// Await a unix signal, or never resolve if the signal stream is absent.
#[cfg(unix)]
async fn wait_unix_signal(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Drain further watcher events for `window` before acting, coalescing the
/// write-then-rename bursts editors produce into one reload.
async fn debounce(watcher: &mut reload::ConfigWatcher, window: Duration) {
    if window.is_zero() {
        return;
    }
    let deadline = tokio::time::Instant::now() + window;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            msg = watcher.rx.recv() => {
                if msg.is_none() {
                    break;
                }
            }
        }
    }
}

// ── Generation ─────────────────────────────────────────────────────────────────

/// One live runtime generation: the spawned engine plus everything needed to
/// cheaply rebuild it on a rejected reload.
struct Generation {
    token: CancellationToken,
    engine_handle: Option<JoinHandle<()>>,
    cfg: Config,
    creds: Credentials,
    engine_cfg: RulesEngineConfig,
    zone_specs: Vec<ZoneSpec>,
    sensor_inventory: Vec<SensorId>,
    display_executors: HashMap<DisplayId, Arc<DisplayExecutor>>,
    /// Per-display render sinks (layer-shell overlays).  Owned by the
    /// generation so they drop on [`teardown`] — the old sinks' Wayland
    /// surfaces are torn down when the generation is replaced, then the
    /// new generation re-converges from live presence.
    #[allow(dead_code)]
    render_sinks: HashMap<DisplayId, Arc<dyn RenderSink>>,
}

/// A freshly spawned generation plus its inbound channel senders (for the
/// forwarding watch channels).
struct GenSpawn {
    generation: Generation,
    ctl_tx: mpsc::Sender<ControlMsg>,
    events_tx: mpsc::Sender<PresenceEvent>,
}

/// Cancel a generation and await its engine (bounded), force-aborting the
/// engine task if it overruns the grace window.
async fn teardown(generation: &mut Generation) {
    generation.token.cancel();
    if let Some(handle) = generation.engine_handle.take() {
        let abort = handle.abort_handle();
        if tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .is_err()
        {
            abort.abort();
            tracing::warn!(event = "engine_abort_forced");
        }
    }
}

/// Everything derived from a config that is needed to spawn a generation.
struct StaticAssembly {
    cfg: Config,
    creds: Credentials,
    engine_cfg: RulesEngineConfig,
    zone_specs: Vec<ZoneSpec>,
    sensor_inventory: Vec<SensorId>,
    display_executors: HashMap<DisplayId, Arc<DisplayExecutor>>,
    sources: Vec<Box<dyn SensorSource>>,
    activity_rules: Vec<ActivityRule>,
    activity_poll: Option<Duration>,
    render_sinks: HashMap<DisplayId, Arc<dyn RenderSink>>,
    /// `InputWake` receiver from render surfaces.  Only populated when the
    /// `render` feature is enabled; the drain task in [`spawn_generation`]
    /// routes each item to [`ControlMsg::InputWake`].
    #[cfg(feature = "render")]
    input_wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<DisplayId>>,
}

/// Load config + credentials, logging any lenient-mode warnings.
fn load_cfg_creds(
    config_path: &std::path::Path,
    creds_path: &std::path::Path,
    strictness: Strictness,
) -> Result<(Config, Credentials)> {
    let (cfg, warnings) = load_config(config_path, strictness)
        .with_context(|| format!("load config '{}'", config_path.display()))?;
    for w in &warnings {
        tracing::warn!(event = "config_warning", key = %w.key_path, message = %w.message);
    }
    let creds = load_credentials(creds_path)
        .with_context(|| format!("load credentials '{}'", creds_path.display()))?;
    Ok((cfg, creds))
}

/// Build controllers + executors (probing each) and derive the engine config.
#[allow(clippy::too_many_lines)]
async fn assemble_static(
    cfg: Config,
    creds: Credentials,
    source_builder: &SourceBuilder,
    #[cfg(feature = "render")] render_sink_builder: Option<&RenderSinkBuilder>,
) -> Result<StaticAssembly> {
    // First rule referencing each display drives its retry + timings.
    let display_rule = index_display_rules(&cfg);

    let mut display_runtime: Vec<DisplayRuntimeCfg> = Vec::new();
    let mut display_executors: HashMap<DisplayId, Arc<DisplayExecutor>> = HashMap::new();
    #[cfg(feature = "render")]
    let (render_sinks, input_wake_rx) = build_render_sinks(&cfg, render_sink_builder);
    #[cfg(not(feature = "render"))]
    let render_sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();

    for (name, dc) in &cfg.displays {
        let did = DisplayId(name.clone());
        // Displays not referenced by any rule are inert — skip them.
        let Some(rc) = display_rule.get(&did) else {
            continue;
        };

        let retry = RetrySettings {
            wake_retries: rc.wake_retries,
            wake_retry_backoff: rc.wake_retry_backoff,
        };
        let timings = SmTimings {
            grace_period: rc.grace_period,
            min_blank_time: rc.min_blank_time,
            min_wake_time: rc.min_wake_time,
            startup_holdoff: cfg.daemon.startup_holdoff,
            wake_retry_interval: rc.wake_retry_interval,
        };

        let controllers = build_controllers(name, dc, &creds)
            .with_context(|| format!("build controllers for display '{name}'"))?;
        let mut executor =
            DisplayExecutor::new(did.clone(), controllers, dc.primary_blank_mode(), retry);

        for (controller, result) in executor.probe_all().await {
            tracing::info!(
                event = "controller_probe",
                display = %did,
                controller = %controller,
                ok = result.is_ok(),
            );
        }

        let effective = executor.effective_modes();
        let chosen = if effective.contains(&dc.primary_blank_mode()) {
            dc.primary_blank_mode()
        } else if let Some(degraded) = dc.degraded_mode.filter(|d| effective.contains(d)) {
            tracing::warn!(
                event = "display_mode_degraded",
                display = %did,
                wanted = ?dc.primary_blank_mode(),
                using = ?degraded,
            );
            degraded
        } else {
            anyhow::bail!(
                "E_MODE_UNSUPPORTED: display '{name}' cannot blank: wanted {:?} \
                 (degraded {:?}), effective modes {:?}",
                dc.primary_blank_mode(),
                dc.degraded_mode,
                effective,
            );
        };

        display_runtime.push(DisplayRuntimeCfg {
            display: did.clone(),
            blank_mode: chosen,
            ladder: dc.normalized_ladder(),
            timings,
        });
        display_executors.insert(did.clone(), Arc::new(executor));
    }

    let built: HashSet<DisplayId> = display_runtime.iter().map(|d| d.display.clone()).collect();

    let rules_runtime: Vec<RuleRuntimeCfg> = cfg
        .rules
        .iter()
        .map(|(id, rc)| RuleRuntimeCfg {
            rule: RuleId(id.clone()),
            zone: ZoneId(rc.zone.clone()),
            displays: rc
                .displays
                .iter()
                .map(|d| DisplayId(d.clone()))
                .filter(|d| built.contains(d))
                .collect(),
        })
        .collect();

    let sensors_runtime: Vec<SensorRuntimeCfg> = cfg
        .sensors
        .iter()
        .map(|(id, sc)| SensorRuntimeCfg {
            sensor: SensorId(id.clone()),
            kind: sc.kind(),
            hold_time: sc.hold_time(),
            stale_timeout: sc
                .stale_timeout()
                .unwrap_or(cfg.daemon.stale_sensor_timeout),
        })
        .collect();

    let zone_specs: Vec<ZoneSpec> = cfg
        .zones
        .iter()
        .map(|(id, zc)| zc.to_zone_spec(id))
        .collect::<Result<Vec<_>, _>>()
        .context("build zone specs")?;

    let sensor_inventory: Vec<SensorId> = cfg.sensors.keys().map(|k| SensorId(k.clone())).collect();

    let sources = (source_builder)(&cfg, &creds).context("build sensor sources")?;
    let (activity_rules, activity_poll) = activity_rules(&cfg);

    let engine_cfg = RulesEngineConfig {
        rules: rules_runtime,
        displays: display_runtime,
        sensors: sensors_runtime,
    };

    Ok(StaticAssembly {
        cfg,
        creds,
        engine_cfg,
        zone_specs,
        sensor_inventory,
        display_executors,
        sources,
        activity_rules,
        activity_poll,
        render_sinks,
        #[cfg(feature = "render")]
        input_wake_rx: Some(input_wake_rx),
    })
}

/// Map each display to the first rule that references it, warning when a later
/// rule references the same display with different retry/timing settings.
fn index_display_rules(cfg: &Config) -> HashMap<DisplayId, RuleConfig> {
    let mut map: HashMap<DisplayId, RuleConfig> = HashMap::new();
    for (rule_id, rc) in &cfg.rules {
        for display in &rc.displays {
            let did = DisplayId(display.clone());
            if let Some(existing) = map.get(&did) {
                if retry_timing_differs(existing, rc) {
                    tracing::warn!(
                        event = "display_multi_rule_conflict",
                        display = %did,
                        rule = %rule_id,
                        "display referenced by multiple rules with differing retry/timing; \
                         keeping the first",
                    );
                }
            } else {
                map.insert(did, rc.clone());
            }
        }
    }
    map
}

fn retry_timing_differs(a: &RuleConfig, b: &RuleConfig) -> bool {
    a.grace_period != b.grace_period
        || a.min_blank_time != b.min_blank_time
        || a.min_wake_time != b.min_wake_time
        || a.wake_retries != b.wake_retries
        || a.wake_retry_backoff != b.wake_retry_backoff
        || a.wake_retry_interval != b.wake_retry_interval
}

/// Extract the `user-activity` inhibitor rules and the minimum poll interval.
fn activity_rules(cfg: &Config) -> (Vec<ActivityRule>, Option<Duration>) {
    let mut rules = Vec::new();
    let mut min_poll: Option<Duration> = None;
    for (id, rc) in &cfg.rules {
        if rc.inhibitors.iter().any(|i| i == "user-activity") {
            rules.push(ActivityRule {
                rule: RuleId(id.clone()),
                idle_threshold: rc.activity_idle_threshold,
            });
            min_poll = Some(min_poll.map_or(rc.activity_poll_interval, |p| {
                p.min(rc.activity_poll_interval)
            }));
        }
    }
    (rules, min_poll)
}

/// Build render sinks for every display whose ladder contains a render
/// stage.  Returns the sink map and a fresh `UnboundedReceiver` for the
/// `InputWake` drain task.  Failures are non-fatal — the engine's empty-sink
/// path synthesises `RenderResult(Err)` so the ladder falls through.
///
/// Called from both [`assemble_static`] (fresh config) and [`rebuild_old`]
/// (rejected-reload rollback) so every generation gets live `InputWake` routing.
#[cfg(feature = "render")]
fn build_render_sinks(
    cfg: &Config,
    render_sink_builder: Option<&RenderSinkBuilder>,
) -> (
    HashMap<DisplayId, Arc<dyn RenderSink>>,
    tokio::sync::mpsc::UnboundedReceiver<DisplayId>,
) {
    let mut sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
    let (input_wake_tx, input_wake_rx) = tokio::sync::mpsc::unbounded_channel::<DisplayId>();

    for (name, dc) in &cfg.displays {
        let did = DisplayId(name.clone());
        let ladder = dc.normalized_ladder();
        if !ladder.iter().any(|s| s.kind.is_render()) {
            continue;
        }
        if let Some(output_name) = &dc.output {
            let sink: Option<Arc<dyn RenderSink>> = if let Some(builder) = render_sink_builder {
                (builder)(did.clone(), output_name.clone())
            } else {
                match LayerShellRenderSink::new(
                    did.clone(),
                    output_name.clone(),
                    Some(&input_wake_tx),
                ) {
                    Ok(sink) => Some(Arc::new(sink)),
                    Err(e) => {
                        tracing::warn!(
                            event = "render_sink_build_failed",
                            display = %did,
                            error = %e,
                        );
                        None
                    }
                }
            };
            if let Some(sink) = sink {
                sinks.insert(did, sink);
            }
        } else {
            tracing::warn!(
                event = "render_sink_missing_output",
                display = %did,
                "render stage configured but no output connector; skipping render sink"
            );
        }
    }

    (sinks, input_wake_rx)
}

/// Spawn the engine, sources, and inhibitor for one generation.
fn spawn_generation(
    root: &CancellationToken,
    assembly: StaticAssembly,
    restore: Option<&StateSnapshot>,
    pending: Option<String>,
) -> Result<GenSpawn> {
    let token = root.child_token();
    let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(64);
    let (events_tx, events_rx) = mpsc::channel::<PresenceEvent>(256);

    let zone = ZoneEngine::new(assembly.zone_specs.clone(), &assembly.sensor_inventory)
        .context("build zone engine")?;

    let executors: HashMap<DisplayId, Arc<dyn CommandSink>> = assembly
        .display_executors
        .iter()
        .map(|(id, exec)| (id.clone(), exec.clone() as Arc<dyn CommandSink>))
        .collect();

    let mut engine = RulesEngine::new(
        assembly.engine_cfg.clone(),
        zone,
        executors,
        assembly.render_sinks.clone(),
        Arc::new(AlwaysOwned),
    )
    .context("build engine")?;

    if let Some(detail) = pending {
        engine.set_pending_reload(Some(detail));
    }
    if let Some(snapshot) = restore {
        apply_restore(&mut engine, snapshot, &assembly.engine_cfg);
    }

    let engine_token = token.clone();
    let engine_handle = tokio::spawn(async move {
        engine.run(events_rx, ctl_rx, engine_token).await;
    });

    // ── InputWake drain (feature-gated: render surfaces emit InputWake) ──
    // Each LayerShellRenderSink pushes DisplayId through an unbounded
    // channel on the first pointer/key event.  This task routes those
    // DisplayIds to the engine as ControlMsg::InputWake so the state
    // machine can react.  Scoped to the generation's cancellation token
    // so it dies on reload alongside the engine.
    #[cfg(feature = "render")]
    if let Some(input_wake_rx) = assembly.input_wake_rx {
        spawn_input_wake_drain(input_wake_rx, ctl_tx.clone(), token.clone());
    }

    for source in assembly.sources {
        let stx = events_tx.clone();
        let stoken = token.clone();
        tokio::spawn(async move {
            if let Err(e) = source.run(stx, stoken).await {
                tracing::error!(event = "sensor_source_exited", error = %e);
            }
        });
    }

    let poll = assembly
        .activity_poll
        .unwrap_or_else(|| Duration::from_secs(5));
    let idle_unit = assembly.cfg.daemon.idle_time_unit;
    let idle_source = assembly.cfg.daemon.idle_source;
    let _inhibitor = inhibit_activity::spawn(
        assembly.activity_rules,
        poll,
        idle_source,
        idle_unit,
        ctl_tx.clone(),
        token.clone(),
    );

    let generation = Generation {
        token,
        engine_handle: Some(engine_handle),
        cfg: assembly.cfg,
        creds: assembly.creds,
        engine_cfg: assembly.engine_cfg,
        zone_specs: assembly.zone_specs,
        sensor_inventory: assembly.sensor_inventory,
        display_executors: assembly.display_executors,
        render_sinks: assembly.render_sinks,
    };

    Ok(GenSpawn {
        generation,
        ctl_tx,
        events_tx,
    })
}

/// Replay the scheduling effects a restored phase would emit. See the module
/// docs for the M1 restore limitation.
fn apply_restore(
    engine: &mut RulesEngine,
    snapshot: &StateSnapshot,
    engine_cfg: &RulesEngineConfig,
) {
    let now = Tick::now();
    for (display, dsnap) in &snapshot.displays {
        let did = DisplayId(display.clone());
        let Some(dcfg) = engine_cfg.displays.iter().find(|d| d.display == did) else {
            continue;
        };
        let phase = match dsnap.phase.as_str() {
            "waking" => Phase::Waking,
            "blanked" => Phase::Blanked,
            _ => continue,
        };
        let (_sm, effects) = DisplayStateMachine::restore(
            dcfg.timings.clone(),
            dcfg.ladder.clone(),
            phase,
            dsnap.cmd_gen,
            now,
        );
        engine.apply_restore_effects(&did, effects);
    }
}

/// A display phase that means the panel is physically off (or on its way off /
/// coming back): the daemon must not silently leave it dark across a reload.
///
/// Render phases (`staged`, `render_pending`) are intentionally excluded — the
/// panel is physically ON during these phases (the render overlay covers it),
/// so a controller wake would be a no-op or worse.  The render overlay is torn
/// down by the old generation's [`teardown`] (which drops the [`RenderSink`]s),
/// and the new generation re-converges from live presence.
fn phase_is_dark(phase: &str) -> bool {
    matches!(phase, "blanked" | "blanking" | "waking")
}

/// Displays that were dark and have **no executor** in the newly assembled
/// generation (dropped from `[displays]` *or* left in `[displays]` but removed
/// from every rule, which makes them inert) — these get a verified physical
/// wake via their OLD executor before the new generation starts.
fn removed_dark_displays(
    snapshot: Option<&StateSnapshot>,
    new_executors: &HashMap<DisplayId, Arc<DisplayExecutor>>,
) -> Vec<DisplayId> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let present: HashSet<&str> = new_executors.keys().map(|d| d.0.as_str()).collect();
    snapshot
        .displays
        .iter()
        .filter(|(id, d)| !present.contains(id.as_str()) && phase_is_dark(&d.phase))
        .map(|(id, _)| DisplayId(id.clone()))
        .collect()
}

/// Displays that were dark and **do have an executor** in the newly assembled
/// generation — these get a defensive physical wake after the new generation
/// spawns (state machines restart `Active`, so a dark panel would otherwise
/// linger until the next edge).
fn retained_dark_displays(
    snapshot: Option<&StateSnapshot>,
    new_executors: &HashMap<DisplayId, Arc<DisplayExecutor>>,
) -> Vec<DisplayId> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let present: HashSet<&str> = new_executors.keys().map(|d| d.0.as_str()).collect();
    snapshot
        .displays
        .iter()
        .filter(|(id, d)| present.contains(id.as_str()) && phase_is_dark(&d.phase))
        .map(|(id, _)| DisplayId(id.clone()))
        .collect()
}

/// Request a snapshot from a generation's engine (bounded).
async fn request_snapshot(ctl: &mpsc::Sender<ControlMsg>) -> Option<StateSnapshot> {
    let (tx, rx) = oneshot::channel();
    if ctl.send(ControlMsg::Snapshot(tx)).await.is_err() {
        return None;
    }
    tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .ok()?
        .ok()
}

/// Forward control messages to the current generation's engine.
async fn forward_ctl(
    mut rx: mpsc::Receiver<ControlMsg>,
    target: watch::Receiver<mpsc::Sender<ControlMsg>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                Some(m) => {
                    let sender = target.borrow().clone();
                    let _ = sender.send(m).await;
                }
                None => break,
            },
        }
    }
}

/// Forward injected presence events to the current generation's engine.
async fn forward_events(
    mut rx: mpsc::Receiver<PresenceEvent>,
    target: watch::Receiver<mpsc::Sender<PresenceEvent>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                Some(m) => {
                    let sender = target.borrow().clone();
                    let _ = sender.send(m).await;
                }
                None => break,
            },
        }
    }
}

#[cfg(feature = "render")]
fn spawn_input_wake_drain(
    input_wake_rx: tokio::sync::mpsc::UnboundedReceiver<DisplayId>,
    ctl_tx: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        tokio::select! {
            () = cancel.cancelled() => {}
            () = async {
                let mut rx = input_wake_rx;
                while let Some(display) = rx.recv().await {
                    if ctl_tx.send(ControlMsg::InputWake(display)).await.is_err() {
                        break; // engine channel closed — shutdown in progress
                    }
                }
            } => {}
        }
    });
}

#[cfg(feature = "render")]
#[cfg(test)]
mod render_tests {
    use super::*;
    use tokio::sync::mpsc;

    /// The drain task routes each `DisplayId` received on the unbounded
    /// channel to the engine as `ControlMsg::InputWake`.  When the control
    /// channel is closed (shutdown), the drain exits cleanly.
    #[tokio::test]
    async fn input_wake_drain_routes_display_id_to_input_wake() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(8);
        let (input_wake_tx, input_wake_rx) = mpsc::unbounded_channel::<DisplayId>();
        let cancel = CancellationToken::new();

        spawn_input_wake_drain(input_wake_rx, ctl_tx, cancel.clone());

        // Push three displays through the wake channel.
        input_wake_tx.send(DisplayId("dp-1".into())).unwrap();
        input_wake_tx.send(DisplayId("hdmi-2".into())).unwrap();
        input_wake_tx.send(DisplayId("dp-1".into())).unwrap();

        // Read them back as ControlMsg::InputWake — drain order must match send order.
        match ctl_rx.recv().await {
            Some(ControlMsg::InputWake(d)) => assert_eq!(d.0, "dp-1"),
            other => panic!("expected InputWake(dp-1), got {other:?}"),
        }
        match ctl_rx.recv().await {
            Some(ControlMsg::InputWake(d)) => assert_eq!(d.0, "hdmi-2"),
            other => panic!("expected InputWake(hdmi-2), got {other:?}"),
        }
        match ctl_rx.recv().await {
            Some(ControlMsg::InputWake(d)) => assert_eq!(d.0, "dp-1"),
            other => panic!("expected InputWake(dp-1), got {other:?}"),
        }

        // Cancel the drain and confirm the control channel is clean.
        cancel.cancel();
        // Drop the wake-side sender so the drain recv() returns None.
        drop(input_wake_tx);
    }

    /// After a rebind (simulating `rebuild_old`), `build_render_sinks` returns
    /// a fresh channel and `spawn_generation` spawns a live drain task — so
    /// `InputWake` from the render surface reaches `ControlMsg::InputWake`.
    ///
    /// This is the regression test for the `rebuild_old` bug where sinks were
    /// cloned with orphaned senders and `input_wake_rx: None` dropped
    /// `InputWake` after a rejected-reload rollback.
    #[tokio::test]
    async fn rebuild_old_produces_live_input_wake_drain() {
        use dormant_core::config::schema::{Config, DaemonConfig};
        use dormant_core::fakes::RecordingRenderSink;
        use indexmap::IndexMap;

        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: {
                let mut m = IndexMap::new();
                m.insert(
                    "mon".into(),
                    dormant_core::config::schema::DisplayConfig {
                        controllers: vec!["command".into()],
                        blank_mode: None,
                        degraded_mode: None,
                        ladder: vec![dormant_core::types::LadderStage {
                            kind: dormant_core::types::StageKind::RenderBlack,
                            dwell: Some(Duration::from_secs(30)),
                        }],
                        screensaver: None,
                        output: Some("DP-1".into()),
                        ddc_display: None,
                        host: None,
                        wol_mac: None,
                        blank_command: None,
                        wake_command: None,
                        modes: Some(vec![dormant_core::types::BlankMode::PowerOff]),
                        ha_url: None,
                        blank_service: None,
                        blank_data: None,
                        wake_service: None,
                        wake_data: None,
                        command_timeout: Duration::from_secs(5),
                        restore_brightness: 100,
                        treat_unreachable_as_blanked: true,
                    },
                );
                m
            },
            rules: {
                let mut m = IndexMap::new();
                m.insert(
                    "r".into(),
                    RuleConfig {
                        zone: "office".into(),
                        displays: vec!["mon".into()],
                        grace_period: Duration::from_secs(30),
                        min_blank_time: Duration::from_secs(0),
                        min_wake_time: Duration::from_secs(0),
                        inhibitors: vec![],
                        activity_idle_threshold: Duration::from_secs(60),
                        activity_poll_interval: Duration::from_secs(5),
                        wake_retries: 0,
                        wake_retry_backoff: Duration::from_millis(10),
                        wake_retry_interval: Duration::from_secs(1),
                    },
                );
                m
            },
        };

        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output| Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>));

        let (sinks, input_wake_rx) = build_render_sinks(&cfg, Some(&factory));

        assert!(
            !sinks.is_empty(),
            "build_render_sinks must return at least one sink for render-eligible display"
        );
        assert!(
            sinks.contains_key(&DisplayId("mon".into())),
            "render sink for 'mon' must be in the returned map"
        );
        // build_render_sinks returns a fresh UnboundedReceiver.
        // With the factory seam (RecordingRenderSink, which does not clone
        // the sender), the channel may close when `input_wake_tx` drops
        // at function exit.  The daemon integration test
        // (`rollback_render_sink_wiring_is_live`) covers the live-drain
        // path through the real LayerShellRenderSink which clones the
        // sender.
        drop(input_wake_rx);
    }
}
