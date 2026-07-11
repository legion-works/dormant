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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;

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
use dormant_core::zone::{ZoneEngine, ZoneSpec, absent_mqtt_hazards};
use dormant_displays::ddc_lock::PanelLocks;
use dormant_displays::executor::{DisplayExecutor, RetrySettings};
use dormant_displays::registry::{build_controllers, capabilities};
use dormant_doctor::DoctorService;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "render")]
use dormant_render::LayerShellRenderSink;

use crate::boot_guard::{self, PromoteVerdict};
use crate::inhibit_activity::{self, ActivityRule};
use crate::notifier::{self, NotifierDeps, NotifySink, NotifyState};
use crate::reload;
use crate::sd_notify::{self, SdNotify};

/// Builds the daemon-lifetime notification sink. Production defaults to
/// [`notifier::ZbusSink`]; tests inject a factory returning a shared
/// recording fake (`with_notify_sink_builder` — `source_builder` precedent).
type NotifySinkBuilder = Arc<dyn Fn() -> Arc<dyn NotifySink> + Send + Sync>;

/// Builds the sensor sources for a config. Production uses the sensor
/// registry; tests inject a factory that returns scripted fakes.
type SourceBuilder =
    Arc<dyn Fn(&Config, &Credentials) -> Result<Vec<Box<dyn SensorSource>>> + Send + Sync>;

/// Builds render sinks for a display.  Production uses
/// [`LayerShellRenderSink`]; tests inject a factory that returns
/// [`RecordingRenderSink`](dormant_core::fakes::RecordingRenderSink).
///
/// The factory receives the display id, output connector name, and
/// an optional `UnboundedSender<DisplayId>` — the same sender
/// passed to [`LayerShellRenderSink::new`] so test factories can
/// capture it and simulate `InputWake` events.  Return `None` to skip
/// the sink (fall-through path).
#[cfg(feature = "render")]
type RenderSinkBuilder = Arc<
    dyn Fn(
            DisplayId,
            String,
            Option<&tokio::sync::mpsc::UnboundedSender<DisplayId>>,
            Option<&dormant_render::ScreensaverSettings>,
        ) -> Option<Arc<dyn RenderSink>>
        + Send
        + Sync,
>;

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
    notify_sink_builder: NotifySinkBuilder,
    disable_ipc: bool,
    sd_notify: SdNotify,
    /// Test seam (T4): overrides the watchdog probe-arm's tick period,
    /// otherwise `sd_notify::watchdog_interval_from_env().unwrap_or(30s)`
    /// (spec §6.3). `WATCHDOG_USEC` is process-global and forbidden in
    /// tests (Global Constraints), so a real cadence test needs a way to
    /// shrink the tick period without touching env — mirrors
    /// `with_sd_notify`'s injection shape.
    watchdog_interval: Option<Duration>,
    /// Test seam (F1, T4 reviewer finding): forces the ACCEPTED-config
    /// `spawn_generation` call in `Runner::reload` (the one after
    /// `load_and_assemble`/`validate` already passed) to fail, so a test
    /// can drive the `before_rebuild_old_spawn_failure` boundary + the
    /// `rebuild_old` recovery path. No config-only seam reaches this call
    /// site: `ZoneEngine::new` runs the SAME deterministic construction
    /// `validate()` already ran on the identical `cfg`, and
    /// `RulesEngine::new`'s only fallible check (a rule's display missing
    /// from the built executor/machine maps) can't fire because
    /// `assemble_static` is fail-fast — an incomplete map never reaches
    /// here. This mirrors the `SdNotify::from_socket_for_test` precedent
    /// (R2-M8): an internal seam, gated the same way, for a real path a
    /// black-box config edit provably cannot reach.
    #[cfg(any(test, feature = "test-util"))]
    force_reload_spawn_failure: bool,
}

/// The default production [`NotifySinkBuilder`]: a fresh [`notifier::ZbusSink`]
/// per call (constructed exactly once, in `App::start`).
fn default_notify_sink_builder() -> NotifySinkBuilder {
    Arc::new(|| Arc::new(notifier::ZbusSink::new()) as Arc<dyn NotifySink>)
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
            notify_sink_builder: default_notify_sink_builder(),
            disable_ipc: false,
            sd_notify: SdNotify::from_env(),
            watchdog_interval: None,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_spawn_failure: false,
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
            notify_sink_builder: default_notify_sink_builder(),
            disable_ipc: false,
            sd_notify: SdNotify::from_env(),
            watchdog_interval: None,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_spawn_failure: false,
        })
    }

    /// Set an injected render-sink factory (test seam).
    ///
    /// When set, `assemble_static` calls this factory instead of
    /// building [`LayerShellRenderSink`] directly.  The factory receives
    /// the display id, output connector name, an optional
    /// `UnboundedSender<DisplayId>` (the `InputWake` channel), and an
    /// optional [`dormant_render::ScreensaverSettings`]; return `None` to skip the sink
    /// (fall-through).
    #[cfg(feature = "render")]
    #[must_use]
    pub fn with_render_sink_builder<F>(mut self, factory: F) -> Self
    where
        F: Fn(
                DisplayId,
                String,
                Option<&tokio::sync::mpsc::UnboundedSender<DisplayId>>,
                Option<&dormant_render::ScreensaverSettings>,
            ) -> Option<Arc<dyn RenderSink>>
            + Send
            + Sync
            + 'static,
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

    /// Inject an [`SdNotify`] (test seam / explicit override — spec §6.2,
    /// §10 R2-M8). When not called, `App::build`/`build_with_sources`
    /// already default to `SdNotify::from_env()`, so production callers
    /// never need this.
    #[must_use]
    pub fn with_sd_notify(mut self, sd: SdNotify) -> Self {
        self.sd_notify = sd;
        self
    }

    /// Override the watchdog probe-arm's tick period (test seam — spec
    /// §6.3). Production leaves this unset and `Runner` falls back to
    /// `sd_notify::watchdog_interval_from_env().unwrap_or(30s)`; a cadence
    /// test that needs a short LKG `stability_window` to elapse inside the
    /// gate's real-time budget calls this instead of touching the
    /// process-global `WATCHDOG_USEC` (forbidden in tests).
    #[must_use]
    pub fn with_watchdog_interval(mut self, interval: Duration) -> Self {
        self.watchdog_interval = Some(interval);
        self
    }

    /// Force the accepted-config `spawn_generation` call inside
    /// `Runner::reload` to fail on every subsequent reload (test seam, F1 —
    /// see the field doc on `App::force_reload_spawn_failure` for why no
    /// config-only seam reaches this call site).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_force_reload_spawn_failure(mut self) -> Self {
        self.force_reload_spawn_failure = true;
        self
    }

    /// Set an injected notification-sink factory (test seam — the
    /// `source_builder` precedent). Production defaults to
    /// [`notifier::ZbusSink`]; tests typically capture a shared `Arc` and
    /// have the factory clone it, so the SAME recording fake instance is
    /// used across every reload generation (mirrors how [`NotifyState`] is
    /// daemon-lifetime, not per-generation).
    #[must_use]
    pub fn with_notify_sink_builder<F>(mut self, factory: F) -> Self
    where
        F: Fn() -> Arc<dyn NotifySink> + Send + Sync + 'static,
    {
        self.notify_sink_builder = Arc::new(factory);
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

        // The daemon's ONE process-wide panel-lock registry (spec §4.3):
        // constructed here, threaded into every `assemble_static` call
        // (this one and every subsequent reload via `Runner`), and never
        // reconstructed for the life of the process — a physical panel's
        // lock is the same `Arc<PanelLock>` across every generation swap.
        let panel_locks = PanelLocks::new();

        // Daemon-lifetime notifier state + sink (spec §4.4): constructed
        // once here and threaded into every `spawn_generation` call (this
        // one and every subsequent reload/rollback via `Runner`), so the
        // notifier's open episodes — and the underlying `ZbusSink`'s cached
        // DBus connection — survive a config reload, exactly like
        // `panel_locks` above.
        let notify_state: Arc<Mutex<NotifyState>> = Arc::new(Mutex::new(NotifyState::default()));
        let notify_sink: Arc<dyn NotifySink> = (self.notify_sink_builder)();

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
            &panel_locks,
        )
        .await
        .context("assemble initial runtime")?;
        #[cfg(not(feature = "render"))]
        let assembly = assemble_static(cfg, creds, &self.source_builder, &panel_locks)
            .await
            .context("assemble initial runtime")?;

        let spawn = spawn_generation(
            &root,
            assembly,
            None,
            None,
            notify_state.clone(),
            notify_sink.clone(),
        )?;

        // Executor-map watch channel for the wear tracker (spec §4.3):
        // seeded here from the first generation, republished by
        // `Runner::install_generation` on every subsequent install/rollback,
        // and emptied (`send_replace(Arc::new(HashMap::new()))`) immediately
        // before teardown in `Runner::reload` so the tracker never calls
        // into a dead executor mid-swap.
        let executors0: HashMap<DisplayId, Arc<dyn CommandSink>> = spawn
            .generation
            .display_executors
            .iter()
            .map(|(id, exec)| (id.clone(), exec.clone() as Arc<dyn CommandSink>))
            .collect();
        let (executors_tx, executors_rx) = watch::channel(Arc::new(executors0));

        // The daemon's single wear-ledger map (spec §5) — shared with the
        // tracker and, in future, IPC/WebUI readers.
        let wear_handle: dormant_core::wear::WearHandle =
            Arc::new(std::sync::RwLock::new(HashMap::new()));

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

        // Wear tracker: daemon-lifetime, reads config via watch, publishes
        // over the front ctl channel (rides `forward_ctl`'s
        // `deliver_or_drop` across generation swaps), sees the current
        // generation's executors via the watch seeded above.
        let _wear_tracker = crate::wear_tracker::spawn(crate::wear_tracker::WearTrackerDeps {
            config_rx: config_rx.clone(),
            ctl_tx: front_ctl_tx.clone(),
            executors_rx: executors_rx.clone(),
            handle: wear_handle.clone(),
            cancel: root.clone(),
        });

        if cfg_clone.daemon.web_allow_nonloopback {
            tracing::warn!(
                event = "web_nonloopback_enabled",
                bind = %cfg_clone.daemon.web_bind,
                "web UI bound off-loopback + UNAUTHENTICATED — doctor/reload/command endpoints are LAN-reachable"
            );
        }

        // Absent-policy + mqtt hazard warns (spec R3-H): a broker hiccup or
        // LWT flap only ever produces `SensorState::Unavailable`, never a
        // real absence, but an `unavailable_policy = "absent"` zone treats
        // Unavailable as Absent — surface every such pairing at startup so
        // an operator can catch the aggressive-blanking foot-gun before it
        // fires.
        for (zone, sensor) in absent_mqtt_hazards(&cfg_clone) {
            tracing::warn!(
                event = "unavailable_absent_mqtt",
                zone = %zone,
                sensor = %sensor,
                "mqtt sensor in an absent-policy zone — a broker/LWT hiccup will be treated as absence"
            );
        }

        let watcher =
            reload::config_watcher(&self.config_path).context("install config file watcher")?;

        // The watchdog probe-arm's tick period (spec §6.3): captured ONCE
        // here so a mid-run env change never re-times the interval arm.
        // Production reads `WATCHDOG_USEC` (halved by `sd_notify`) or falls
        // back to 30s; `with_watchdog_interval` overrides for tests.
        let watchdog_interval = self.watchdog_interval.unwrap_or_else(|| {
            sd_notify::watchdog_interval_from_env().unwrap_or(Duration::from_secs(30))
        });

        // The first LKG candidate tracks this boot's generation (spec §4
        // Mechanism: "set at startup completion"). `None` when
        // `watchdog.lkg_enabled` is false — no candidate tracking, no
        // files written, ever, until the config re-enables it on a reload.
        let lkg_candidate =
            new_lkg_candidate(&self.config_path, cfg_clone.watchdog.lkg_enabled, "boot");

        let runner = Runner {
            config_path: self.config_path.clone(),
            creds_path: self.creds_path.clone(),
            strictness: self.strictness,
            source_builder: self.source_builder,
            #[cfg(feature = "render")]
            render_sink_builder: self.render_sink_builder,
            root: root.clone(),
            engine_ctl: engine_ctl_tx,
            engine_events: engine_events_tx,
            executors_tx,
            reload_tx: reload_tx.clone(),
            config_tx,
            creds_tx,
            generation: spawn.generation,
            started_web_port,
            started_web_bind,
            panel_locks,
            notify_state,
            notify_sink,
            sd: self.sd_notify,
            watchdog_interval,
            lkg_candidate,
            lkg_defer_count: 0,
            probe_failed_warned: false,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_spawn_failure: self.force_reload_spawn_failure,
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
                    creds_path: self.creds_path.clone(),
                    apply_lock: tokio::sync::Mutex::new(()),
                    doctor: doctor_service.clone(),
                    wear: wear_handle.clone(),
                    web_bind: addr,
                    cancel: root.clone(),
                    reload_timeout: std::time::Duration::from_secs(10),
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
    /// Current generation's executor map for the wear tracker (spec §4.3).
    /// Republished on every install/rollback via [`Runner::install_generation`];
    /// emptied immediately before teardown in [`Runner::reload`].
    executors_tx: watch::Sender<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    reload_tx: broadcast::Sender<ReloadOutcome>,
    config_tx: watch::Sender<Arc<Config>>,
    creds_tx: watch::Sender<Arc<Credentials>>,
    generation: Generation,
    /// Port the web UI was started with (for reload change-detection).
    started_web_port: Option<u16>,
    /// Bind address the web UI was started with (for reload change-detection).
    started_web_bind: std::net::IpAddr,
    /// The daemon's single process-wide panel-lock registry (spec §4.3),
    /// constructed once in [`App::start`] and carried by `Runner` across
    /// every reload so `load_and_assemble`'s `assemble_static` call always
    /// reuses the SAME registry — a physical panel's lock must resolve to
    /// the same `Arc<PanelLock>` whether it came from the old generation's
    /// controller or the new one's.
    panel_locks: Arc<PanelLocks>,
    /// Daemon-lifetime notifier episode state (spec §4.4), constructed once
    /// in [`App::start`] and threaded unchanged into every generation's
    /// [`spawn_generation`] call — episodes survive a config reload.
    notify_state: Arc<Mutex<NotifyState>>,
    /// Daemon-lifetime notification sink, constructed once in
    /// [`App::start`] and threaded unchanged into every generation.
    notify_sink: Arc<dyn NotifySink>,
    /// The systemd watchdog sender (spec §6.2/§6.3). Injected via
    /// [`App::with_sd_notify`]; defaults to [`SdNotify::from_env`].
    sd: SdNotify,
    /// The probe arm's tick period (spec §6.3), captured once at
    /// construction — see [`App::with_watchdog_interval`].
    watchdog_interval: Duration,
    /// The in-flight LKG promotion candidate (spec §4), or `None` when
    /// `watchdog.lkg_enabled` is false (no candidate tracking at all) or no
    /// candidate has been armed yet. Set at startup and at every successful
    /// reload; cleared on a successful promotion write.
    lkg_candidate: Option<LkgCandidate>,
    /// Consecutive display-health-deferred promotion ticks (spec §4 R2-M3
    /// starvation cap; R3-M2: counts ANY unhealthy set, not just a fixed
    /// one — see [`boot_guard::should_promote`]). Reset on a successful
    /// promotion or a fully-healthy candidate tick.
    lkg_defer_count: u32,
    /// Whether `watchdog_probe_failed` has already been logged for the
    /// CURRENT run of consecutive probe failures (spec §6.3: warn once per
    /// failure streak, not once per failed tick).
    probe_failed_warned: bool,
    /// Test seam (F1) — see `App::force_reload_spawn_failure`.
    #[cfg(any(test, feature = "test-util"))]
    force_reload_spawn_failure: bool,
}

/// One LKG promotion candidate (spec §4 Mechanism): the config bytes
/// captured when the generation this candidate tracks became live, and the
/// wall-clock instant it started running. `bytes` is the "candidate copy"
/// spec §4 point 2 compares fresh reads of the live config path against, to
/// detect an un-applied direct edit sitting on disk (`lkg_skipped_dirty`).
struct LkgCandidate {
    bytes: Vec<u8>,
    since: Instant,
    /// Where this candidate's generation came from — carried into the
    /// `.meta.json` sidecar's `source` field on promotion (spec §3:
    /// `"boot"|"reload"`).
    source: &'static str,
    /// One-shot log latches (spec §4: several promotion-gate events are
    /// "warn once per candidate", not once per tick).
    dirty_logged: bool,
    health_deferred_logged: bool,
    save_failed_logged: bool,
}

/// Build the initial (or post-reload) LKG candidate for `config_path`, or
/// `None` when `lkg_enabled` is false (spec §4 failure semantics: "no
/// candidate tracking, no files written" — the gate is checked ONCE here,
/// not re-checked every tick, so a mid-run config edit that disables
/// tracking only takes effect at the next reload, matching how every other
/// `Runner` field derived from `self.generation.cfg` behaves).
fn new_lkg_candidate(
    config_path: &std::path::Path,
    lkg_enabled: bool,
    source: &'static str,
) -> Option<LkgCandidate> {
    if !lkg_enabled {
        return None;
    }
    let bytes = std::fs::read(config_path).ok()?;
    Some(LkgCandidate {
        bytes,
        since: Instant::now(),
        source,
        dirty_logged: false,
        health_deferred_logged: false,
        save_failed_logged: false,
    })
}

/// `last-known-good.meta.json` sidecar shape (spec §3 — advisory only; the
/// LKG file itself is the source of truth, loaded directly via
/// `load_config`).
#[derive(Serialize)]
struct LkgMeta {
    schema_version: u32,
    fingerprint: boot_guard::Fingerprint,
    saved_at_epoch_s: u64,
    source: &'static str,
}

impl Runner {
    fn install_generation(&mut self, spawn: GenSpawn) {
        let _ = self.engine_ctl.send(spawn.ctl_tx);
        let _ = self.engine_events.send(spawn.events_tx);
        let executors: HashMap<DisplayId, Arc<dyn CommandSink>> = spawn
            .generation
            .display_executors
            .iter()
            .map(|(id, exec)| (id.clone(), exec.clone() as Arc<dyn CommandSink>))
            .collect();
        self.executors_tx.send_replace(Arc::new(executors));
        self.generation = spawn.generation;
    }

    /// Reload the config, restarting the runtime in place. See the module
    /// docs for the full state machine.
    #[allow(clippy::too_many_lines)]
    async fn reload(&mut self) {
        let old_ctl = self.engine_ctl.borrow().clone();
        let preliminary = request_snapshot(&old_ctl).await;

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

        // Step-boundary ping 1/7 (spec §6.3): after `load_and_assemble`
        // returns (probes done). The spec names this as a boundary DISTINCT
        // from "after the quiesce loop" below — the serial controller
        // probes inside `load_and_assemble` and the quiesce loop that
        // follows are two independently-unbounded stretches of work, so
        // each needs its own ping to keep either gap alone bounded.
        self.ping("after_assemble");

        // Build the set of rule-driven displays from the NEW config.
        // Rule-less (manual-only) displays are those in [displays] but NOT
        // referenced by any rule.
        let ruled: HashSet<DisplayId> = index_display_rules(&new_assembly.cfg)
            .keys()
            .cloned()
            .collect();

        // ── Quiesce rule-less displays caught mid-blank ──────────────────────
        // A rule-less display with phase "blanking" has no result driver after
        // teardown.  Samsung KEY_PICTURE_OFF is a TOGGLE (re-issuing could
        // invert the panel), so we must NOT restore a naked Blanking.  Instead,
        // poll the still-live engine until the phase reaches a terminal state
        // ("blanked" / "active") or the deadline passes.
        let quiesce_deadline =
            tokio::time::Instant::now() + dormant_core::config::defaults::COMMAND_TIMEOUT;
        let mut snapshot = preliminary;
        if let Some(ref snap) = snapshot {
            let need_quiesce: HashSet<String> = snap
                .displays
                .iter()
                .filter(|(id, d)| {
                    let did = DisplayId((*id).clone());
                    classify_transient(&d.phase, !ruled.contains(&did)) == TransientClass::Quiesce
                })
                .map(|(id, _)| id.clone())
                .collect();
            if !need_quiesce.is_empty() {
                tracing::info!(
                    event = "reload_quiesce_blanking",
                    count = need_quiesce.len(),
                    "rule-less display(s) caught mid-blank; polling until terminal"
                );
                loop {
                    let all_terminal = if let Some(ref s) = snapshot {
                        need_quiesce.iter().all(|id| {
                            s.displays
                                .iter()
                                .find(|(did, _)| did == id)
                                .is_none_or(|(_, d)| {
                                    matches!(d.phase.as_str(), "blanked" | "active")
                                })
                        })
                    } else {
                        false
                    };
                    if all_terminal {
                        break;
                    }
                    if tokio::time::Instant::now() >= quiesce_deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if let Some(s) = request_snapshot(&old_ctl).await {
                        snapshot = Some(s);
                    }
                }
                let stuck_count = snapshot.as_ref().map_or(0, |s| {
                    need_quiesce
                        .iter()
                        .filter(|id| {
                            s.displays
                                .iter()
                                .find(|(did, _)| did == *id)
                                .is_some_and(|(_, d)| d.phase.as_str() == "blanking")
                        })
                        .count()
                });
                if stuck_count > 0 {
                    tracing::warn!(
                        event = "reload_quiesce_timeout",
                        count = stuck_count,
                        "rule-less display(s) still blanking at deadline; defensive-waking"
                    );
                }
            }
        }

        // Step-boundary ping 2/7 (spec §6.3): after the quiesce loop above
        // (whether or not any rule-less display actually needed quiescing)
        // — the second of the two DISTINCT probes-done / quiesce-done
        // boundaries the spec names; fired unconditionally so the boundary
        // exists even on a reload with nothing to quiesce.
        self.ping("after_quiesce");

        let removed = removed_dark_displays(snapshot.as_ref(), &new_assembly.display_executors);
        let retained_dark =
            retained_dark_displays(snapshot.as_ref(), &new_assembly.display_executors, &ruled);

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

        // Fail-closed during the swap: the wear tracker must never call into
        // an executor that is about to be torn down (spec §4.3). Empty the
        // watch BEFORE teardown so a tracker tick racing this window sees no
        // executors (skips its round) rather than a stale/dying one.
        self.executors_tx.send_replace(Arc::new(HashMap::new()));

        // Step-boundary ping 3/7 (spec §6.3 F5): right before the teardown +
        // verified-wake work below, so that gap too is independently
        // bounded (on top of the after_assemble/after_quiesce boundaries
        // above covering the two stretches that precede this point).
        self.ping("before_teardown");

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
                    // Step-boundary ping 5/7 (spec §6.3/P10): before the
                    // `rebuild_old` recovery rebuild (`spawn_generation` +
                    // engine construction, controllers reused — not a
                    // controller reprobe, but still non-trivial work).
                    self.ping("before_rebuild_old_wake_failure");
                    self.rebuild_old(Some(detail.clone()), snapshot.as_ref());
                    let _ = self.reload_tx.send(ReloadOutcome::Rejected(detail));
                    return;
                }
                tracing::info!(event = "reload_removed_display_woken", display = %display_id);
                // Step-boundary ping 4/7 (spec §6.3 F5/R2-M5): one ping PER
                // removed display inside this loop — the serial verified-wake
                // burst is unbounded in display count, so bounding every gap
                // at one display's worst-case burst requires a ping here,
                // not just before/after the whole loop.
                self.ping("removed_display_wake");
            }
        }

        // Dispatch-relevant voiding gate (spec R3-M6): a display whose
        // blank/wake command construction changed between `self.generation.cfg`
        // (old, still live here — `install_generation` hasn't run yet) and
        // `new_cfg` carries no meaningful failure evidence forward, so zero
        // its `wake_attempts`/`last_blank_failed` before `apply_restore` seeds
        // them into the new generation. Only the ACCEPTED-spawn path below
        // applies the gate; `rebuild_old`'s rollback callers restart the SAME
        // (old) config, so their snapshot needs no filtering.
        //
        // The sensor `reported` voiding gate (spec R3-S) is a sibling filter
        // applied on top: it zeroes a sensor's carried "has reported" bit
        // when the sensor's own config changed between the same two configs.
        // Both filters run only over the accepted-spawn snapshot; `rebuild_old`
        // sites below keep the ORIGINAL unfiltered `snapshot`.
        let restore_snapshot = snapshot.as_ref().map(|snap| {
            let displays_filtered =
                reload::zero_changed_displays(snap, &self.generation.cfg, &new_cfg);
            reload::zero_changed_sensor_reported(&displays_filtered, &self.generation.cfg, &new_cfg)
        });

        let spawn_result = spawn_generation(
            &self.root,
            new_assembly,
            restore_snapshot.as_ref(),
            None,
            self.notify_state.clone(),
            self.notify_sink.clone(),
        );
        // Test seam (F1): see `App::force_reload_spawn_failure` doc — no
        // config-only path reaches an `Err` here, so a test that needs to
        // pin `before_rebuild_old_spawn_failure` + the `rebuild_old`
        // recovery flips this flag and gets the REAL `Err(e)` arm below,
        // just with a synthetic cause.
        #[cfg(any(test, feature = "test-util"))]
        let spawn_result = if self.force_reload_spawn_failure {
            spawn_result.and_then(|_| {
                Err(anyhow::anyhow!(
                    "test-injected spawn_generation failure (F1 test-util seam)"
                ))
            })
        } else {
            spawn_result
        };
        match spawn_result {
            Ok(spawn) => {
                self.install_generation(spawn);
                // Re-check the hazard pairing against the NEW config on every
                // accepted reload (mirrors the startup check in
                // `App::start`) — an edit can introduce (or remove) an
                // absent-policy + mqtt pairing just as easily as a fresh
                // boot can.
                for (zone, sensor) in absent_mqtt_hazards(&new_cfg) {
                    tracing::warn!(
                        event = "unavailable_absent_mqtt",
                        zone = %zone,
                        sensor = %sensor,
                        "mqtt sensor in an absent-policy zone — a broker/LWT hiccup will be treated as absence"
                    );
                }
                // Combine retained rule-driven dark displays with stuck rule-less
                // blanking displays — both need a physical wake because the new
                // machines start Active.
                let mut wake_list = retained_dark;
                // Re-derive stuck from the final snapshot (the quiesce loop may
                // have advanced some to terminal, but any still-blanking ones
                // need waking).
                if let Some(ref snap) = snapshot {
                    let stuck: Vec<DisplayId> = snap
                        .displays
                        .iter()
                        .filter(|(id, d)| {
                            let did = DisplayId((*id).clone());
                            d.phase.as_str() == "blanking" && !ruled.contains(&did)
                        })
                        .map(|(id, _)| DisplayId(id.clone()))
                        .collect();
                    // Retained (ruled) and stuck (!ruled) are disjoint by
                    // construction — no dedup needed.
                    wake_list.extend(stuck);
                }
                self.defensive_wake(wake_list);
                self.config_tx.send_replace(Arc::new(new_cfg));
                self.creds_tx.send_replace(Arc::new(new_creds));
                tracing::info!(event = "config_reloaded");
                // New generation, new LKG candidate (spec §4: "cleared/reset
                // by any reload" — a reload restarts the stability window
                // for the NEW config). `watchdog.lkg_enabled` is read from
                // the just-installed config, so a reload can also arm
                // tracking that was off before.
                self.lkg_defer_count = 0;
                self.lkg_candidate = new_lkg_candidate(
                    &self.config_path,
                    self.generation.cfg.watchdog.lkg_enabled,
                    "reload",
                );
                // Step-boundary ping 7/7 (spec §6.3): reload end. Fires
                // BEFORE the reload-outcome broadcast (not after): on the
                // real multi-threaded runtime a receiver parked on
                // `subscribe_reload()` can wake and run in TRUE PARALLEL
                // with the rest of this function the instant `send` is
                // called, so anything after `send` races an observer that
                // reacts to the outcome — the ping must be fully visible
                // BEFORE the outcome is, not after.
                self.ping("reload_end");
                let _ = self.reload_tx.send(ReloadOutcome::Reloaded);
            }
            Err(e) => {
                let detail = format!("rebuild from new config failed: {e}");
                tracing::error!(event = "config_reload_rejected", detail = %detail);
                // Step-boundary ping 6/7 (spec §6.3/P10): before the second
                // `rebuild_old` call site (the accepted-config
                // `spawn_generation` failure path).
                self.ping("before_rebuild_old_spawn_failure");
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
                &self.panel_locks,
            )
            .await
            .map_err(|e| e.to_string())
        }
        #[cfg(not(feature = "render"))]
        {
            assemble_static(cfg, creds, &self.source_builder, &self.panel_locks)
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
        match spawn_generation(
            &self.root,
            assembly,
            snapshot,
            pending,
            self.notify_state.clone(),
            self.notify_sink.clone(),
        ) {
            Ok(spawn) => self.install_generation(spawn),
            Err(e) => tracing::error!(event = "reload_rebuild_old_failed", error = %e),
        }
    }

    /// Send `WATCHDOG=1` and log a `step`-tagged marker (test/ops
    /// observability — `sd_notify::watchdog`'s wire payload is always the
    /// literal `WATCHDOG=1`; the `step` field is how tests and journal
    /// readers tell the seven in-reload boundaries apart, spec §6.3/P9).
    /// Info level (not debug): `reload()` boundaries fire at most a handful
    /// of times per reload, not per periodic tick, so the volume is the
    /// same order as the `reload_*` events already logged at info here.
    fn ping(&mut self, step: &'static str) {
        self.sd.watchdog();
        tracing::info!(event = "watchdog_ping", step = %step);
    }

    /// The watchdog probe-arm tick (spec §6.3): probe the engine via the
    /// SAME `request_snapshot` idiom `reload()` uses, ping only on success,
    /// and run the §4 LKG-promotion check on every healthy tick.
    async fn watchdog_tick(&mut self) {
        let ctl = self.engine_ctl.borrow().clone();
        let probe_result = watchdog_probe(&ctl).await;
        if ping_if_healthy(&mut self.sd, probe_result.as_ref()) {
            self.probe_failed_warned = false;
            // `ping_if_healthy` returning `true` only on `Some` is the
            // whole point of the extraction below — safe to unwrap.
            self.lkg_tick(
                probe_result
                    .as_ref()
                    .expect("ping_if_healthy true implies Some"),
            );
        } else {
            // A wedged engine starves the watchdog by design (spec
            // invariant #3) — NO ping — and any in-flight LKG candidate
            // loses its unbroken-healthy-window claim (spec F3).
            reset_candidate_on_probe_failure(&mut self.lkg_candidate, Instant::now());
            if !self.probe_failed_warned {
                tracing::warn!(
                    event = "watchdog_probe_failed",
                    "engine did not answer a snapshot round-trip; watchdog ping withheld"
                );
                self.probe_failed_warned = true;
            }
        }
    }

    /// The §4 LKG-promotion check, run on every healthy probe tick. A
    /// `None` candidate (disabled, or none armed yet) is a no-op — spec
    /// §4 failure semantics: `lkg_enabled = false` means no candidate
    /// tracking, no files, ever.
    fn lkg_tick(&mut self, snapshot: &StateSnapshot) {
        let Some(since) = self.lkg_candidate.as_ref().map(|c| c.since) else {
            return;
        };
        let window = self.generation.cfg.watchdog.stability_window;
        // Read fresh, THEN compare — kept as two owned values (not a
        // borrow held across the match below) so the verdict handlers are
        // free to take `&mut self.lkg_candidate`/`&self` without conflict.
        let fresh_bytes = std::fs::read(&self.config_path).ok();
        let on_disk_matches = matches!(
            (&fresh_bytes, self.lkg_candidate.as_ref()),
            (Some(b), Some(c)) if *b == c.bytes
        );

        let verdict = boot_guard::should_promote(
            since,
            Instant::now(),
            window,
            snapshot,
            self.lkg_defer_count,
            on_disk_matches,
        );

        match verdict {
            PromoteVerdict::Wait => {}
            PromoteVerdict::DeferHealth => {
                self.lkg_defer_count += 1;
                if let Some(candidate) = self.lkg_candidate.as_mut()
                    && !candidate.health_deferred_logged
                {
                    tracing::warn!(
                        event = "lkg_deferred_display_health",
                        "LKG promotion deferred: at least one display's controllers are all unhealthy"
                    );
                    candidate.health_deferred_logged = true;
                }
            }
            PromoteVerdict::SkipDirty => {
                if let Some(candidate) = self.lkg_candidate.as_mut()
                    && !candidate.dirty_logged
                {
                    tracing::warn!(
                        event = "lkg_skipped_dirty",
                        "LKG promotion skipped: on-disk config no longer matches the running candidate"
                    );
                    candidate.dirty_logged = true;
                }
            }
            PromoteVerdict::Promote | PromoteVerdict::PromoteDespiteHealth => {
                if verdict == PromoteVerdict::PromoteDespiteHealth {
                    tracing::warn!(
                        event = "lkg_promoted_with_unhealthy_display",
                        "LKG promoted despite an unhealthy display: deferral cap reached"
                    );
                }
                let source = self.lkg_candidate.as_ref().map_or("boot", |c| c.source);
                // F4 (TOCTOU re-read fix): `on_disk_matches` being true is
                // the ONLY way to reach this arm (`should_promote`'s dirty
                // check gates it — see the doc above), which guarantees
                // `fresh_bytes` is `Some`. Reuse those already-read bytes
                // instead of re-reading `config_path` a second time: a
                // second read would race a concurrent edit landing in the
                // gap between the dirty check above and the write below,
                // silently promoting bytes that were never validated
                // against the candidate at all. One read per tick, period.
                let Some(bytes) = fresh_bytes.as_ref() else {
                    // Unreachable given should_promote's contract (dirty
                    // check requires Some on both sides), but fail closed
                    // (retry next tick) rather than panic if it ever isn't.
                    return;
                };
                match write_lkg(source, bytes) {
                    Ok(()) => {
                        tracing::info!(event = "lkg_saved");
                        self.lkg_defer_count = 0;
                        self.lkg_candidate = None;
                    }
                    Err(e) => {
                        if let Some(candidate) = self.lkg_candidate.as_mut()
                            && !candidate.save_failed_logged
                        {
                            tracing::warn!(event = "lkg_save_failed", error = %e);
                            candidate.save_failed_logged = true;
                        }
                        // Retry next tick (spec §4 failure semantics) —
                        // candidate stays armed, `since` untouched.
                    }
                }
            }
        }
    }
}

/// Atomically copy `bytes` (the config bytes the caller already read fresh
/// for this tick's dirty check — F4: no second read here, closing the
/// TOCTOU window a re-read would otherwise open) to
/// `state_dir()/last-known-good.toml` + the `.meta.json` sidecar (spec §3).
/// A free function, not a `Runner` method (`clippy::unused_self`): it needs
/// nothing from `Runner` beyond the two arguments the caller already has.
fn write_lkg(source: &'static str, bytes: &[u8]) -> std::io::Result<()> {
    let dir = dormant_core::paths::state_dir();
    boot_guard::write_atomic_bytes(&dir, "last-known-good.toml", bytes)?;
    let meta = LkgMeta {
        schema_version: 1,
        fingerprint: boot_guard::fingerprint_bytes(bytes),
        saved_at_epoch_s: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        source,
    };
    boot_guard::write_atomic_json(&dir, "last-known-good.meta.json", &meta)
}

/// The watchdog probe (spec §6.3 F8): proves the engine drains its ctl
/// mailbox via the SAME `request_snapshot` idiom `reload()` uses. Extracted
/// as a plain function taking the ctl sender — not a `Runner` method — so a
/// test can hand it a manufactured closed channel directly (P11) without
/// needing to wedge a real running engine.
async fn watchdog_probe(ctl: &mpsc::Sender<ControlMsg>) -> Option<StateSnapshot> {
    request_snapshot(ctl).await
}

/// The probe→ping decision (spec §6.3 invariant #3, F3 reviewer finding
/// m6: "ping even when the probe fails" must be impossible). Extracted as a
/// free fn over a plain `&mut SdNotify` — not a `Runner` method — so a unit
/// test can pin BOTH halves ("healthy probe → a `WATCHDOG=1` datagram is
/// sent" AND "failed probe → no datagram, ever") against a real
/// `UnixDatagram` receiver via [`SdNotify::from_socket_for_test`], without
/// constructing a full `Runner` (which `watchdog_tick`'s cadence-stop
/// behavior otherwise has no honest seam to reach from outside — closing
/// the T4 review's m6 survivor).
///
/// Returns whether a ping was sent, so the caller (`watchdog_tick`) can
/// gate its own healthy-path bookkeeping (clearing `probe_failed_warned`,
/// running the LKG tick) on the SAME decision this function made, instead
/// of re-deriving it from `probe_result` a second time.
fn ping_if_healthy(sd: &mut SdNotify, probe_result: Option<&StateSnapshot>) -> bool {
    if probe_result.is_some() {
        sd.watchdog();
        true
    } else {
        false
    }
}

/// Reset an LKG candidate's window start on a failed engine probe (spec §4
/// F3: "any failed probe RESETS the candidate window"). Factored out of
/// [`Runner::watchdog_tick`] so the reset rule is unit-testable without a
/// full `Runner`/`App` — the reset is explicitly the CALLER's job, not
/// `should_promote`'s (spec's pure gate only ever sees an unbroken window).
fn reset_candidate_on_probe_failure(candidate: &mut Option<LkgCandidate>, now: Instant) {
    if let Some(c) = candidate.as_mut() {
        c.since = now;
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
    // The watchdog probe arm (spec §6.3): a plain interval, period captured
    // once at `Runner` construction (`App::start`/`with_watchdog_interval`).
    // Runs on BOTH platform branches below — on non-Unix the tick still
    // drives §4 LKG promotion even though `sd.watchdog()` itself is a
    // permanent no-op there (no `NOTIFY_SOCKET` concept off Linux).
    let mut watchdog_ticker = tokio::time::interval(runner.watchdog_interval);
    watchdog_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                _ = watchdog_ticker.tick() => {
                    runner.watchdog_tick().await;
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
                _ = watchdog_ticker.tick() => {
                    runner.watchdog_tick().await;
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
///
/// `locks` is the daemon's single process-wide [`PanelLocks`] registry
/// (spec §4.3) — constructed once in [`App::start`] and reused, unchanged,
/// across every reload generation (see [`Runner::panel_locks`]), so a
/// physical panel's lock is the same `Arc<PanelLock>` before and after a
/// config reload.
#[allow(clippy::too_many_lines)]
async fn assemble_static(
    cfg: Config,
    creds: Credentials,
    source_builder: &SourceBuilder,
    #[cfg(feature = "render")] render_sink_builder: Option<&RenderSinkBuilder>,
    locks: &Arc<PanelLocks>,
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
        // A display named by no rule is MANUAL-ONLY: build it so
        // it is controllable by hand; no zone drives it.  (Was: skipped
        // as inert.)
        let (retry, timings) = match display_rule.get(&did) {
            Some(rc) => (
                RetrySettings {
                    wake_retries: rc.wake_retries,
                    wake_retry_backoff: rc.wake_retry_backoff,
                },
                SmTimings {
                    grace_period: rc.grace_period,
                    min_blank_time: rc.min_blank_time,
                    min_wake_time: rc.min_wake_time,
                    startup_holdoff: cfg.daemon.startup_holdoff,
                    wake_retry_interval: rc.wake_retry_interval,
                },
            ),
            None => (
                RetrySettings {
                    wake_retries: dormant_core::config::defaults::WAKE_RETRIES,
                    wake_retry_backoff: dormant_core::config::defaults::WAKE_RETRY_BACKOFF,
                },
                DisplayRuntimeCfg::manual_defaults(cfg.daemon.startup_holdoff),
            ),
        };

        let controllers = build_controllers(name, dc, &creds, locks)
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
///
/// ## Screensaver playlist assembly
///
/// The playlist is built at assembly time (startup/reload), not per-show —
/// this keeps FS scanning off the wayland thread and matches the generation
/// model where a config reload rebuilds it.  Fresh-per-show is future work.
#[cfg(feature = "render")]
fn build_render_sinks(
    cfg: &Config,
    render_sink_builder: Option<&RenderSinkBuilder>,
) -> (
    HashMap<DisplayId, Arc<dyn RenderSink>>,
    tokio::sync::mpsc::UnboundedReceiver<DisplayId>,
) {
    use dormant_render::ScreensaverSettings;
    use dormant_render::playlist;

    let mut sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
    let (input_wake_tx, input_wake_rx) = tokio::sync::mpsc::unbounded_channel::<DisplayId>();

    for (name, dc) in &cfg.displays {
        let did = DisplayId(name.clone());
        let ladder = dc.normalized_ladder();
        if !ladder.iter().any(|s| s.kind.is_render()) {
            continue;
        }

        // Build ScreensaverSettings at assembly time so FS scanning
        // stays off the wayland thread.
        let screensaver_settings: Option<ScreensaverSettings> = if ladder
            .iter()
            .any(|s| s.kind == dormant_core::types::StageKind::RenderScreensaver)
        {
            dc.screensaver.as_ref().map(|ss| {
                let items = playlist::build_playlist(&ss.source, None);
                // scale_mode: None (absent) → Fill.  Validation has already
                // rejected any unknown string value, so a failed parse here
                // would only be reachable through a programmatic caller
                // bypassing validate; in that case we still default to Fill
                // rather than refuse to build the sink.
                let scale_mode = ss
                    .scale_mode
                    .as_deref()
                    .map(dormant_render::ScaleMode::from_config_str)
                    .transpose()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                // transition: None (absent) → Crossfade.  Validation has
                // already rejected any unknown string value; the parse
                // fallback mirrors the scale_mode path above (default
                // rather than refuse).
                let transition = ss
                    .transition
                    .as_deref()
                    .map(dormant_render::TransitionMode::from_config_str)
                    .transpose()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                // transition_duration: None (absent) → 1 second default
                // (matches `defaults::TRANSITION_DURATION` so the
                // ScreensaverSettings default is the single source of
                // truth).  Validation has already rejected any value
                // outside the 100 ms ..= 10 s bound.
                let transition_duration = ss
                    .transition_duration
                    .unwrap_or(ScreensaverSettings::default().transition_duration);
                ScreensaverSettings {
                    items,
                    image_duration: ScreensaverSettings::default().image_duration,
                    audio: ss.audio,
                    scale_mode,
                    transition,
                    transition_duration,
                }
            })
        } else {
            None
        };

        if let Some(output_name) = &dc.output {
            let ss_ref = screensaver_settings.as_ref();
            let sink: Option<Arc<dyn RenderSink>> = if let Some(builder) = render_sink_builder {
                (builder)(
                    did.clone(),
                    output_name.clone(),
                    Some(&input_wake_tx),
                    ss_ref,
                )
            } else {
                match LayerShellRenderSink::new(
                    did.clone(),
                    output_name.clone(),
                    Some(&input_wake_tx),
                ) {
                    Ok(sink) => {
                        if let Some(ref settings) = screensaver_settings {
                            sink.set_screensaver(settings.clone());
                        }
                        Some(Arc::new(sink))
                    }
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

/// Spawn the engine, sources, inhibitor, and notifier for one generation.
///
/// `notify_state` / `notify_sink` are daemon-lifetime (constructed once in
/// [`App::start`]) — every call site threads the SAME `Arc`s through so the
/// notifier's open episodes (and the sink's cached connection) survive a
/// config reload.
fn spawn_generation(
    root: &CancellationToken,
    assembly: StaticAssembly,
    restore: Option<&StateSnapshot>,
    pending: Option<String>,
    notify_state: Arc<Mutex<NotifyState>>,
    notify_sink: Arc<dyn NotifySink>,
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

    // Desktop wake/blank-failure notifier (spec §4.4) — `notifier::spawn`
    // returns `None` (no-op) when `[notifications] enabled = false`,
    // mirroring `inhibit_activity::spawn`'s own None-returning precedent.
    let _notifier = notifier::spawn(NotifierDeps {
        ctl: ctl_tx.clone(),
        cfg: assembly.cfg.notifications,
        state: notify_state,
        sink: notify_sink,
        cancel: token.clone(),
    });

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

/// Restore display phases across a reload.
///
/// Rule-driven displays replay only the scheduling effects (M1 behavior —
/// the new state machine starts `Active` and the engine re-converges from
/// live presence).  Rule-less displays get their full phase preserved via
/// [`RulesEngine::install_restored_machine`] (M2).
///
/// Transient `"blanking"` phases are skipped defensively — the reload
/// quiesce should prevent a naked `Blanking` from reaching here, but
/// restoring a display mid-toggle (e.g. Samsung `KEY_PICTURE_OFF` which
/// toggles) would be incorrect, so `continue` is the safe default.
fn apply_restore(
    engine: &mut RulesEngine,
    snapshot: &StateSnapshot,
    engine_cfg: &RulesEngineConfig,
) {
    let now = Tick::now();
    // Displays referenced by any rule are rule-driven; all others in
    // [displays] are manual-only (rule-less).
    let ruled: HashSet<&DisplayId> = engine_cfg.rules.iter().flat_map(|r| &r.displays).collect();
    for (display, dsnap) in &snapshot.displays {
        let did = DisplayId(display.clone());
        let Some(dcfg) = engine_cfg.displays.iter().find(|d| d.display == did) else {
            continue;
        };
        // Seed wake-failure bookkeeping BEFORE the phase match below —
        // failure evidence (wake_attempts / last_blank_failed) must carry
        // forward for a display regardless of its restored phase (in
        // particular "active", which the match below `continue`s on).
        // Any dispatch-relevant voiding has already happened upstream (see
        // `reload::zero_changed_displays`, applied by `Runner::reload`
        // before this snapshot reaches `apply_restore`).
        engine.seed_failure_state(&did, dsnap.wake_attempts, dsnap.last_blank_failed);
        #[allow(clippy::match_same_arms)]
        let phase = match dsnap.phase.as_str() {
            "waking" => Phase::Waking,
            "blanked" => Phase::Blanked,
            "blanking" => continue, // never restore a naked Blanking
            _ => continue,
        };
        let (sm, effects) = DisplayStateMachine::restore(
            dcfg.timings.clone(),
            dcfg.ladder.clone(),
            phase,
            dsnap.cmd_gen,
            now,
        );
        if ruled.contains(&did) {
            let _ = sm; // unused in the effects-only path
            engine.apply_restore_effects(&did, effects);
        } else {
            engine.install_restored_machine(&did, sm, effects, now);
        }
    }

    // Seed the sensor `reported` diagnostic forward (sibling seam to the
    // display wake-failure seeding above). Only sensors still present in the
    // NEW engine config are seeded — a sensor dropped from `[sensors]` by
    // the edit that triggered this reload has no runtime slot in the new
    // engine to seed into. Any dispatch-relevant voiding for a RETAINED
    // sensor has already happened upstream (see
    // `reload::zero_changed_sensor_reported`, applied by `Runner::reload`
    // before this snapshot reaches `apply_restore`), so a `true` here is
    // always meaningful for the new sensor identity.
    for ssnap in &snapshot.sensors {
        let sid = SensorId(ssnap.id.clone());
        if !ssnap.reported {
            continue;
        }
        if !engine_cfg.sensors.iter().any(|s| s.sensor == sid) {
            continue;
        }
        engine.seed_sensor_reported(&sid);
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
/// generation (dropped from `[displays]` entirely — its executor no longer
/// exists in the new generation) — these get a verified physical wake via
/// their OLD executor before the new generation starts.
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
///
/// Rule-less (manual-only) displays are excluded — a dark manual-only display
/// reflects operator intent, not a wedge, and its phase is preserved across
/// reload by [`apply_restore`].
fn retained_dark_displays(
    snapshot: Option<&StateSnapshot>,
    new_executors: &HashMap<DisplayId, Arc<DisplayExecutor>>,
    ruled: &HashSet<DisplayId>,
) -> Vec<DisplayId> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let present: HashSet<&str> = new_executors.keys().map(|d| d.0.as_str()).collect();
    snapshot
        .displays
        .iter()
        .filter(|(id, d)| {
            present.contains(id.as_str())
                && phase_is_dark(&d.phase)
                && ruled.contains(&DisplayId((*id).clone()))
        })
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
///
/// During a config reload the engine is torn down and rebuilt; there is a
/// brief window between dropping the old `ControlMsg` receiver and writing
/// the new generation's sender into the watch. A message forwarded in that
/// window hits a dead sender and would be lost without retry — the canonical
/// failure for a manual-only (rule-less) display is a dropped `ForceWake`
/// leaving the screen dark with nothing to re-converge it.
///
/// On `SendError` we recover the message (`SendError(m)` gives it back),
/// wait for the next watch write via `target.changed()`, and re-send —
/// bounded by `MAX_SWAP_WAITS` to cover the pathological case where the
/// reload stalls and no new generation ever arrives. Single-task sequential
/// processing preserves ordering: each `rx.recv()` and its full retry loop
/// complete before the next receive.
async fn forward_ctl(
    mut rx: mpsc::Receiver<ControlMsg>,
    mut target: watch::Receiver<mpsc::Sender<ControlMsg>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                None => break,
                Some(m) => {
                    if !deliver_or_drop(m, &mut target, &cancel, "ctl_forward_dropped").await {
                        break;
                    }
                }
            },
        }
    }
}

/// Deliver a single message to the current generation's engine, retrying
/// across a reload swap on `SendError`. Returns `false` when the forwarder
/// should exit (cancellation observed, watch sender dropped, or
/// `MAX_SWAP_WAITS` exceeded — all logged under `drop_event`).
///
/// Generic over the message type so the control and presence forwarders
/// share one implementation. Each call site passes its own `drop_event`
/// literal (AGENTS.md rule 3 — grep-stable anchors at the definition site):
/// `"ctl_forward_dropped"` for control messages, `"event_forward_dropped"`
/// for presence events. `T` carries no explicit bound — the `Send`
/// requirement comes implicitly from `mpsc::Sender<T>::send(T)` at the
/// call site.
async fn deliver_or_drop<T>(
    msg: T,
    target: &mut watch::Receiver<mpsc::Sender<T>>,
    cancel: &CancellationToken,
    drop_event: &'static str,
) -> bool {
    const MAX_SWAP_WAITS: usize = 2;
    let mut pending = msg;
    let mut waits: usize = 0;
    loop {
        let sender = target.borrow().clone();
        match sender.send(pending).await {
            Ok(()) => return true,
            Err(e) => {
                pending = e.0;
                if waits >= MAX_SWAP_WAITS {
                    tracing::warn!(
                        event = drop_event,
                        waits,
                        "message dropped: no live engine generation within retry bound"
                    );
                    return false;
                }
                waits += 1;
                tokio::select! {
                    () = cancel.cancelled() => {
                        tracing::warn!(
                            event = drop_event,
                            waits,
                            "message dropped: cancellation observed while awaiting new generation"
                        );
                        return false;
                    }
                    r = target.changed() => {
                        if r.is_err() {
                            tracing::warn!(
                                event = drop_event,
                                waits,
                                "message dropped: watch sender dropped (shutdown)"
                            );
                            return false;
                        }
                    }
                }
            }
        }
    }
}

/// Forward injected presence events to the current generation's engine.
///
/// Same reload-window drop risk as [`forward_ctl`]: between
/// `teardown(old)` and `install_generation(new)` the watch still points at
/// the dead old-generation sender, and a presence event forwarded in that
/// window would be silently swallowed by `SendError`. Retry the same way
/// — see [`deliver_or_drop`] — bounded by `MAX_SWAP_WAITS`.
async fn forward_events(
    mut rx: mpsc::Receiver<PresenceEvent>,
    mut target: watch::Receiver<mpsc::Sender<PresenceEvent>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                None => break,
                Some(m) => {
                    if !deliver_or_drop(m, &mut target, &cancel, "event_forward_dropped").await {
                        break;
                    }
                }
            },
        }
    }
}

// ── Reload-time transient classification ──────────────────────────────────────

/// A rule-less display mid-blank must be quiesced (polled to terminal) before
/// restore; terminal dark phases restore directly; everything else is ignored.
#[derive(Debug, PartialEq, Eq)]
enum TransientClass {
    /// Poll the still-live engine until the phase reaches a terminal state or
    /// the deadline passes.
    Quiesce,
    /// Restore the display's phase directly (no quiesce needed).
    RestoreDirect,
    /// No special handling needed — the display is not rule-less or its phase
    /// is not dark.
    Ignore,
}

/// Classify a display snapshot at reload time based on its current phase and
/// whether it is rule-less.  Only rule-less displays in dark phases need
/// handling: `"blanking"` needs quiesce (poll to terminal), `"blanked"` and
/// `"waking"` restore directly, and everything else is ignored.
fn classify_transient(phase: &str, ruleless: bool) -> TransientClass {
    match (phase, ruleless) {
        ("blanking", true) => TransientClass::Quiesce,
        ("blanked" | "waking", true) => TransientClass::RestoreDirect,
        _ => TransientClass::Ignore,
    }
}

// ── Watchdog probe arm + LKG candidate reset (T4) ───────────────────────────

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    /// P11: the probe fn takes the ctl sender directly, so a test can hand
    /// it a manufactured closed channel without wedging a real engine.
    #[tokio::test]
    async fn probe_returns_none_on_closed_ctl_channel() {
        let (tx, rx) = mpsc::channel::<ControlMsg>(1);
        drop(rx);
        assert!(watchdog_probe(&tx).await.is_none());
    }

    #[tokio::test]
    async fn probe_returns_snapshot_on_live_channel() {
        let (tx, mut rx) = mpsc::channel::<ControlMsg>(1);
        tokio::spawn(async move {
            if let Some(ControlMsg::Snapshot(reply)) = rx.recv().await {
                let _ = reply.send(StateSnapshot {
                    sensors: Vec::new(),
                    zones: Vec::new(),
                    displays: Vec::new(),
                    pending_reload: None,
                });
            }
        });
        assert!(watchdog_probe(&tx).await.is_some());
    }

    /// F3 (T4 review): pins the cadence-stop half of the probe→ping
    /// decision that had no test — a closed ctl channel (probe fails) must
    /// produce NO datagram on the wire, not just a `None` return value.
    /// Uses the SAME `from_socket_for_test`/`UnixDatagram` seam the
    /// `daemon_smoke` cadence test uses, at the unit level, so this doesn't
    /// need a full `Runner`/`App`. This is also the re-kill test for
    /// reviewer mutation m6 ("ping unconditionally, ignoring the probe
    /// result") — see the report for the mutation re-application evidence.
    #[test]
    fn ping_if_healthy_sends_nothing_on_failed_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let addr = std::os::unix::net::SocketAddr::from_pathname(&path).unwrap();
        let mut sd = SdNotify::from_socket_for_test(&addr);

        let sent = ping_if_healthy(&mut sd, None);

        assert!(
            !sent,
            "ping_if_healthy must report no ping on a failed probe"
        );
        let mut buf = [0u8; 64];
        assert!(
            listener.recv_from(&mut buf).is_err(),
            "a failed probe must never put a WATCHDOG=1 datagram on the wire"
        );
    }

    /// Companion to the failure-side pin above: a healthy probe result MUST
    /// still ping (guards against an overcorrection that silences the
    /// healthy path too).
    #[test]
    fn ping_if_healthy_sends_watchdog_on_healthy_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let addr = std::os::unix::net::SocketAddr::from_pathname(&path).unwrap();
        let mut sd = SdNotify::from_socket_for_test(&addr);

        let snapshot = StateSnapshot {
            sensors: Vec::new(),
            zones: Vec::new(),
            displays: Vec::new(),
            pending_reload: None,
        };
        let sent = ping_if_healthy(&mut sd, Some(&snapshot));

        assert!(
            sent,
            "ping_if_healthy must report a ping on a healthy probe"
        );
        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }

    /// Spec §4 F3: "any failed probe RESETS the candidate window" — the
    /// reset is the CALLER's job (`watchdog_tick`), not `should_promote`'s;
    /// this pins that the extracted reset helper actually advances `since`.
    #[test]
    fn failed_probe_resets_candidate_since() {
        let far_past = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .unwrap();
        let mut candidate = Some(LkgCandidate {
            bytes: vec![1, 2, 3],
            since: far_past,
            source: "boot",
            dirty_logged: false,
            health_deferred_logged: false,
            save_failed_logged: false,
        });

        let now = Instant::now();
        reset_candidate_on_probe_failure(&mut candidate, now);

        assert_eq!(
            candidate.unwrap().since,
            now,
            "since must be reset to `now`"
        );
    }

    #[test]
    fn failed_probe_reset_is_noop_on_no_candidate() {
        let mut candidate: Option<LkgCandidate> = None;
        // Must not panic when no candidate is armed (lkg_enabled = false,
        // or none set yet).
        reset_candidate_on_probe_failure(&mut candidate, Instant::now());
        assert!(candidate.is_none());
    }

    #[test]
    fn new_lkg_candidate_disabled_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"config_version = 1\n").unwrap();
        assert!(new_lkg_candidate(&path, false, "boot").is_none());
    }

    #[test]
    fn new_lkg_candidate_enabled_captures_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"config_version = 1\nx = 1\n").unwrap();
        let candidate = new_lkg_candidate(&path, true, "reload").expect("candidate armed");
        assert_eq!(candidate.bytes, b"config_version = 1\nx = 1\n");
        assert_eq!(candidate.source, "reload");
    }
}

#[cfg(test)]
mod transient_tests {
    use super::*;

    #[test]
    fn classify_transient_all_combos() {
        // Rule-less, blanking → quiesce
        assert_eq!(
            classify_transient("blanking", true),
            TransientClass::Quiesce
        );
        // Rule-less, blanked → restore directly
        assert_eq!(
            classify_transient("blanked", true),
            TransientClass::RestoreDirect
        );
        // Rule-less, waking → restore directly
        assert_eq!(
            classify_transient("waking", true),
            TransientClass::RestoreDirect
        );
        // Rule-less, active → ignore
        assert_eq!(classify_transient("active", true), TransientClass::Ignore);
        // Rule-less, staged → ignore
        assert_eq!(classify_transient("staged", true), TransientClass::Ignore);
        // Rule-less, render_pending → ignore
        assert_eq!(
            classify_transient("render_pending", true),
            TransientClass::Ignore
        );

        // Rule-driven, blanking → ignore (not transient)
        assert_eq!(
            classify_transient("blanking", false),
            TransientClass::Ignore
        );
        // Rule-driven, blanked → ignore (handled by existing defensive-wake)
        assert_eq!(classify_transient("blanked", false), TransientClass::Ignore);
        // Rule-driven, waking → ignore
        assert_eq!(classify_transient("waking", false), TransientClass::Ignore);
        // Rule-driven, active → ignore
        assert_eq!(classify_transient("active", false), TransientClass::Ignore);

        // Unknown phase, rule-less → ignore
        assert_eq!(classify_transient("garbage", true), TransientClass::Ignore);
        // Unknown phase, rule-driven → ignore
        assert_eq!(classify_transient("garbage", false), TransientClass::Ignore);
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

    /// Build a minimal `DisplayConfig` with sensible defaults for the
    /// render-sink plumbing tests below.  Mirrors the helper used in
    /// `validate::tests::base_display_cfg` but lives here as a test-local
    /// helper to avoid coupling across crates.
    fn base_display_cfg_for_test() -> dormant_core::config::schema::DisplayConfig {
        dormant_core::config::schema::DisplayConfig {
            controllers: Vec::new(),
            blank_mode: None,
            degraded_mode: None,
            ladder: Vec::new(),
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: Duration::from_secs(5),
            restore_brightness: 100,
            samsung_restore_backlight: dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
        }
    }

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

    /// `build_render_sinks` returns a render sink for every render-eligible
    /// display plus a fresh `InputWake` channel receiver.
    ///
    /// This covers only the channel-construction half of the rollback fix.
    /// The live-drain end-to-end path (`rebuild_old` →
    /// `build_render_sinks` → `spawn_generation` spawns a drain →
    /// `ControlMsg::InputWake`) is exercised by the daemon integration test
    /// `rollback_input_wake_routes_through_drain`.
    #[tokio::test]
    async fn build_render_sinks_returns_sink_and_channel_for_render_eligible_display() {
        use dormant_core::config::schema::{Config, DaemonConfig};
        use dormant_core::fakes::RecordingRenderSink;
        use indexmap::IndexMap;

        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
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
                        samsung_restore_backlight:
                            dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
                        treat_unreachable_as_blanked: true,
                        panel_type: dormant_core::wear::PanelType::default(),
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
        let factory: RenderSinkBuilder = Arc::new(move |_did, _output, _tx, _ss| {
            Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>)
        });

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

    /// Full-hop plumbing test: a TOML-parsed config with
    /// `screensaver.scale_mode = "stretch"` propagates through
    /// `build_render_sinks` → factory seam, arriving at the player as
    /// `ScreensaverSettings { scale_mode: ScaleMode::Stretch, .. }`.
    ///
    /// Closes the "silently dropped at a hop" gap (the per-mode tests in
    /// dormant-render verify the enum → mpv property half; this test
    /// proves the config-string → enum half survives the daemon's
    /// `build_render_sinks`).
    #[tokio::test]
    #[cfg(feature = "render")]
    async fn build_render_sinks_passes_scale_mode_stretch_through() {
        use dormant_core::config::schema::{DisplayConfig, ScreensaverConfig, ScreensaverSource};
        use dormant_core::fakes::RecordingRenderSink;
        use dormant_core::types::{LadderStage, StageKind};
        use std::sync::Mutex;

        let cfg = Config {
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    controllers: vec!["kwin-dpms".into()],
                    ladder: vec![
                        LadderStage {
                            kind: StageKind::Controller(dormant_core::types::BlankMode::PowerOff),
                            dwell: Some(Duration::from_secs(30)),
                        },
                        LadderStage {
                            kind: StageKind::RenderScreensaver,
                            dwell: None,
                        },
                    ],
                    screensaver: Some(ScreensaverConfig {
                        trigger: "vacancy".into(),
                        audio: false,
                        source: vec![ScreensaverSource {
                            path: Some("/tmp/img.png".into()),
                            urls: Vec::new(),
                            recurse: false,
                            shuffle: false,
                            order: None,
                            image_duration: None,
                        }],
                        scale_mode: Some("stretch".into()),
                        transition: None,
                        transition_duration: None,
                        shift_px: 2,
                        shift_interval: Duration::from_secs(120),
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
        };

        // Capture the `ScreensaverSettings` the factory receives so the
        // assertion below can verify the scale_mode hop.
        let captured: Arc<Mutex<Option<dormant_render::ScreensaverSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_for_factory = captured.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder = Arc::new(move |_did, _output, _tx, ss| {
            if let Some(s) = ss {
                *captured_for_factory.lock().expect("capture") = Some(s.clone());
            }
            Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>)
        });

        let (sinks, _rx) = build_render_sinks(&cfg, Some(&factory));
        assert!(
            !sinks.is_empty(),
            "RenderScreensaver ladder stage must produce at least one sink"
        );

        let observed = captured
            .lock()
            .expect("capture")
            .take()
            .expect("factory should have received ScreensaverSettings");
        assert_eq!(
            observed.scale_mode,
            dormant_render::ScaleMode::Stretch,
            "scale_mode = \"stretch\" must survive build_render_sinks hop"
        );
    }

    /// Complement to `build_render_sinks_passes_scale_mode_stretch_through`:
    /// an ABSENT `scale_mode` key falls back to the production default
    /// (`ScaleMode::Fill` — OS-screensaver norm).
    #[tokio::test]
    #[cfg(feature = "render")]
    async fn build_render_sinks_defaults_scale_mode_to_fill_when_absent() {
        use dormant_core::config::schema::{DisplayConfig, ScreensaverConfig, ScreensaverSource};
        use dormant_core::fakes::RecordingRenderSink;
        use dormant_core::types::{LadderStage, StageKind};
        use std::sync::Mutex;

        let cfg = Config {
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    controllers: vec!["kwin-dpms".into()],
                    ladder: vec![
                        LadderStage {
                            kind: StageKind::Controller(dormant_core::types::BlankMode::PowerOff),
                            dwell: Some(Duration::from_secs(30)),
                        },
                        LadderStage {
                            kind: StageKind::RenderScreensaver,
                            dwell: None,
                        },
                    ],
                    screensaver: Some(ScreensaverConfig {
                        trigger: "vacancy".into(),
                        audio: false,
                        source: vec![ScreensaverSource {
                            path: Some("/tmp/img.png".into()),
                            urls: Vec::new(),
                            recurse: false,
                            shuffle: false,
                            order: None,
                            image_duration: None,
                        }],
                        // scale_mode intentionally absent (None) — the
                        // build_render_sinks hop must default to Fill.
                        scale_mode: None,
                        transition: None,
                        transition_duration: None,
                        shift_px: 2,
                        shift_interval: Duration::from_secs(120),
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
        };
        assert!(
            cfg.displays["mon"]
                .screensaver
                .as_ref()
                .unwrap()
                .scale_mode
                .is_none(),
            "absent scale_mode must parse as None"
        );

        let captured: Arc<Mutex<Option<dormant_render::ScreensaverSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_for_factory = captured.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder = Arc::new(move |_did, _output, _tx, ss| {
            if let Some(s) = ss {
                *captured_for_factory.lock().expect("capture") = Some(s.clone());
            }
            Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>)
        });

        let (sinks, _rx) = build_render_sinks(&cfg, Some(&factory));
        assert!(!sinks.is_empty());

        let observed = captured
            .lock()
            .expect("capture")
            .take()
            .expect("factory should have received ScreensaverSettings");
        assert_eq!(
            observed.scale_mode,
            dormant_render::ScaleMode::Fill,
            "absent scale_mode must default to Fill at the build_render_sinks hop"
        );
    }

    /// Companion to `build_render_sinks_passes_scale_mode_stretch_through`:
    /// `screensaver.transition = "none"` survives the daemon's
    /// `build_render_sinks` → factory seam hop, arriving at the player
    /// as `ScreensaverSettings { transition: TransitionMode::None, .. }`.
    #[tokio::test]
    #[cfg(feature = "render")]
    async fn build_render_sinks_passes_transition_none_through() {
        use dormant_core::config::schema::{DisplayConfig, ScreensaverConfig, ScreensaverSource};
        use dormant_core::fakes::RecordingRenderSink;
        use dormant_core::types::{LadderStage, StageKind};
        use std::sync::Mutex;

        let cfg = Config {
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    controllers: vec!["kwin-dpms".into()],
                    ladder: vec![
                        LadderStage {
                            kind: StageKind::Controller(dormant_core::types::BlankMode::PowerOff),
                            dwell: Some(Duration::from_secs(30)),
                        },
                        LadderStage {
                            kind: StageKind::RenderScreensaver,
                            dwell: None,
                        },
                    ],
                    screensaver: Some(ScreensaverConfig {
                        trigger: "vacancy".into(),
                        audio: false,
                        source: vec![ScreensaverSource {
                            path: Some("/tmp/img.png".into()),
                            urls: Vec::new(),
                            recurse: false,
                            shuffle: false,
                            order: None,
                            image_duration: None,
                        }],
                        scale_mode: None,
                        transition: Some("none".into()),
                        transition_duration: None,
                        shift_px: 2,
                        shift_interval: Duration::from_secs(120),
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
        };

        let captured: Arc<Mutex<Option<dormant_render::ScreensaverSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_for_factory = captured.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder = Arc::new(move |_did, _output, _tx, ss| {
            if let Some(s) = ss {
                *captured_for_factory.lock().expect("capture") = Some(s.clone());
            }
            Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>)
        });

        let (sinks, _rx) = build_render_sinks(&cfg, Some(&factory));
        assert!(
            !sinks.is_empty(),
            "RenderScreensaver ladder stage must produce at least one sink"
        );

        let observed = captured
            .lock()
            .expect("capture")
            .take()
            .expect("factory should have received ScreensaverSettings");
        assert_eq!(
            observed.transition,
            dormant_render::TransitionMode::None,
            "transition = \"none\" must survive build_render_sinks hop"
        );
    }

    /// Companion to `build_render_sinks_defaults_scale_mode_to_fill_when_absent`:
    /// an ABSENT `transition` key falls back to the production default
    /// (`TransitionMode::Crossfade` — user asked for transitions).
    #[tokio::test]
    #[cfg(feature = "render")]
    async fn build_render_sinks_defaults_transition_to_crossfade_when_absent() {
        use dormant_core::config::schema::{DisplayConfig, ScreensaverConfig, ScreensaverSource};
        use dormant_core::fakes::RecordingRenderSink;
        use dormant_core::types::{LadderStage, StageKind};
        use std::sync::Mutex;

        let cfg = Config {
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    controllers: vec!["kwin-dpms".into()],
                    ladder: vec![
                        LadderStage {
                            kind: StageKind::Controller(dormant_core::types::BlankMode::PowerOff),
                            dwell: Some(Duration::from_secs(30)),
                        },
                        LadderStage {
                            kind: StageKind::RenderScreensaver,
                            dwell: None,
                        },
                    ],
                    screensaver: Some(ScreensaverConfig {
                        trigger: "vacancy".into(),
                        audio: false,
                        source: vec![ScreensaverSource {
                            path: Some("/tmp/img.png".into()),
                            urls: Vec::new(),
                            recurse: false,
                            shuffle: false,
                            order: None,
                            image_duration: None,
                        }],
                        scale_mode: None,
                        // transition intentionally absent (None) —
                        // the build_render_sinks hop must default to
                        // Crossfade (the user asked for transitions).
                        transition: None,
                        transition_duration: None,
                        shift_px: 2,
                        shift_interval: Duration::from_secs(120),
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
        };
        assert!(
            cfg.displays["mon"]
                .screensaver
                .as_ref()
                .unwrap()
                .transition
                .is_none(),
            "absent transition must parse as None"
        );

        let captured: Arc<Mutex<Option<dormant_render::ScreensaverSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_for_factory = captured.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder = Arc::new(move |_did, _output, _tx, ss| {
            if let Some(s) = ss {
                *captured_for_factory.lock().expect("capture") = Some(s.clone());
            }
            Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>)
        });

        let (sinks, _rx) = build_render_sinks(&cfg, Some(&factory));
        assert!(!sinks.is_empty());

        let observed = captured
            .lock()
            .expect("capture")
            .take()
            .expect("factory should have received ScreensaverSettings");
        assert_eq!(
            observed.transition,
            dormant_render::TransitionMode::Crossfade,
            "absent transition must default to Crossfade at the build_render_sinks hop"
        );
    }
}

// ── forward_ctl reload-window tests (#9) ─────────────────────────────────────

#[cfg(test)]
mod forward_ctl_tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Build a watch seeded with a `ControlMsg` sender whose receiver has been
    /// dropped — exactly the state of the engine watch between
    /// `teardown(old)` and `install_generation(new)` during a reload.
    fn dead_watch() -> (
        watch::Sender<mpsc::Sender<ControlMsg>>,
        watch::Receiver<mpsc::Sender<ControlMsg>>,
        mpsc::Sender<ControlMsg>,
    ) {
        let (dead_tx, dead_rx) = mpsc::channel::<ControlMsg>(1);
        drop(dead_rx);
        let (watch_tx, watch_rx) = watch::channel(dead_tx.clone());
        (watch_tx, watch_rx, dead_tx)
    }

    /// RED-FIRST crux (#9): a control message forwarded during the reload
    /// window — when the watch still points at the dead old-generation sender
    /// — must arrive on the NEW live receiver once `install_generation`
    /// writes the replacement sender. With the unfixed
    /// `let _ = sender.send(m).await` the message is dropped and the
    /// assertion times out.
    #[tokio::test]
    async fn forward_ctl_retries_across_generation_swap() {
        let (watch_tx, watch_rx, _dead_tx) = dead_watch();
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel::<ControlMsg>(1);

        let handle = tokio::spawn(forward_ctl(front_rx, watch_rx, cancel.clone()));

        // Send a ForceWake into the front channel. forward_ctl will try the
        // dead sender, get SendError, and start waiting on changed().
        front_tx
            .send(ControlMsg::ForceWake(DisplayId("manual-only".into())))
            .await
            .expect("front channel send");

        // Force a scheduler hand-off so forward_ctl's task is guaranteed to
        // run, consume the front-channel message, attempt the dead-sender
        // send, and park on `changed()` BEFORE we update the watch. Without
        // this yield the multi-thread test runtime can race the watch write
        // ahead of forward_ctl's first `borrow()`, letting the unfixed
        // `let _ = sender.send(m).await` see the new live sender and
        // deliver the message trivially — a polite test that doesn't guard
        // the fix.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Install the new generation's live sender — simulates install_generation.
        let (new_tx, mut new_rx) = mpsc::channel::<ControlMsg>(1);
        watch_tx.send(new_tx).expect("watch send");

        // The fixed forward_ctl must re-deliver the pending message to the
        // NEW receiver. Timeout-bound so a hang becomes a clean assert.
        let received = tokio::time::timeout(Duration::from_secs(2), new_rx.recv())
            .await
            .expect("forward_ctl did not deliver to the new generation within 2s")
            .expect("new generation channel closed unexpectedly");

        match received {
            ControlMsg::ForceWake(d) => assert_eq!(d.0, "manual-only"),
            other => panic!("expected ForceWake(manual-only), got {other:?}"),
        }

        cancel.cancel();
        // Drop the front sender so the spawn can exit.
        drop(front_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// Bounded-retry path: with no live generation ever arriving (a
    /// pathological non-shutdown stall), the message is dropped after
    /// `MAX_SWAP_WAITS` wait cycles and `forward_ctl` exits cleanly. Each
    /// `changed()` wait is resolved by writing another dead sender to the
    /// watch, so the retry loop completes and `forward_ctl` logs
    /// `ctl_forward_dropped` rather than hanging. Crucially, the front
    /// channel is NOT closed and cancellation is NOT triggered — the exit
    /// must come from the `MAX_SWAP_WAITS` bound, not from rx recv / cancel.
    /// Against the unfixed code this test would hang on the next
    /// `rx.recv()` after the dropped message.
    #[tokio::test]
    async fn forward_ctl_drops_after_max_swap_waits_when_no_new_generation() {
        // Mirror the constant bound inside `deliver_or_drop`. Each write
        // lets one `changed()` resolution complete; without a live sender
        // every retry fails and the bound is hit.
        const MAX_SWAP_WAITS: usize = 2;
        let (watch_tx, watch_rx, _dead_tx) = dead_watch();
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel::<ControlMsg>(1);

        let handle = tokio::spawn(forward_ctl(front_rx, watch_rx, cancel.clone()));

        front_tx
            .send(ControlMsg::ForceWake(DisplayId("doomed".into())))
            .await
            .expect("front channel send");

        // Yield so forward_ctl processes the front message and parks on
        // its first `changed()` await before we start writing to the watch.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        for _ in 0..MAX_SWAP_WAITS {
            let (extra_dead_tx, extra_dead_rx) = mpsc::channel::<ControlMsg>(1);
            drop(extra_dead_rx);
            watch_tx.send(extra_dead_tx).expect("watch send");
            // Yield so forward_ctl processes the changed() resolution,
            // re-borrow the dead sender, fail to send, and re-arm the wait.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }

        // No cancellation, front_tx still alive — forward_ctl must still
        // exit because `deliver_or_drop` returns false at the bound and
        // the outer loop breaks. Bound the wait so an unbounded retry
        // loop (regression) or the unfixed "swallow then await forever"
        // path becomes a clean test failure.
        let join = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("forward_ctl did not exit within 2s — MAX_SWAP_WAITS bound was not honored");
        assert!(join.is_ok(), "forward_ctl task panicked: {join:?}");
    }

    /// Cancel mid-wait: while `forward_ctl` is blocked on `changed()` waiting
    /// for the next generation, cancelling the token must break the inner
    /// retry loop and the outer `forward_ctl` cleanly — no hang, no panic.
    #[tokio::test]
    async fn forward_ctl_breaks_cleanly_on_cancel_during_swap_wait() {
        let (_watch_tx, watch_rx, _dead_tx) = dead_watch();
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel::<ControlMsg>(1);

        let handle = tokio::spawn(forward_ctl(front_rx, watch_rx, cancel.clone()));

        // Trigger the dead-sender path so forward_ctl parks on changed().
        front_tx
            .send(ControlMsg::ForceWake(DisplayId("cancelled".into())))
            .await
            .expect("front channel send");

        // Yield so forward_ctl reaches its `changed()` await before we cancel.
        // Without this, on a multi-thread runtime the cancel may race ahead of
        // the task ever entering the changed() wait.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Now cancel — forward_ctl must exit cleanly without ever writing
        // to the watch.
        cancel.cancel();
        drop(front_tx);

        let join = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("forward_ctl did not exit within 1s after cancel");
        assert!(join.is_ok(), "forward_ctl task panicked on cancel");
    }
}

#[cfg(test)]
mod forward_events_tests {
    use super::*;
    use dormant_core::types::{SensorState, Timestamp};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Build a watch seeded with a `PresenceEvent` sender whose receiver has
    /// been dropped — the same state `forward_events` would observe between
    /// `teardown(old)` and `install_generation(new)` during a reload.
    fn dead_watch() -> (
        watch::Sender<mpsc::Sender<PresenceEvent>>,
        watch::Receiver<mpsc::Sender<PresenceEvent>>,
        mpsc::Sender<PresenceEvent>,
    ) {
        let (dead_tx, dead_rx) = mpsc::channel::<PresenceEvent>(1);
        drop(dead_rx);
        let (watch_tx, watch_rx) = watch::channel(dead_tx.clone());
        (watch_tx, watch_rx, dead_tx)
    }

    /// RED-FIRST crux: a presence event forwarded during the reload window —
    /// when the watch still points at the dead old-generation sender — must
    /// arrive on the NEW live receiver once `install_generation` writes the
    /// replacement sender. With the unfixed `let _ = sender.send(m).await`
    /// the message is dropped and the assertion times out.
    #[tokio::test]
    async fn forward_events_retries_across_generation_swap() {
        let (watch_tx, watch_rx, _dead_tx) = dead_watch();
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel::<PresenceEvent>(1);

        let handle = tokio::spawn(forward_events(front_rx, watch_rx, cancel.clone()));

        // Send a Present event into the front channel. forward_events will try
        // the dead sender, get SendError, and start waiting on changed().
        let event = PresenceEvent::new(
            SensorId("test-sensor".into()),
            SensorState::Present,
            Timestamp::now(),
        );
        front_tx
            .send(event.clone())
            .await
            .expect("front channel send");

        // Force a scheduler hand-off so forward_events is guaranteed to
        // consume the front-channel message, attempt the dead-sender send,
        // and park on `changed()` BEFORE we update the watch. Without this
        // yield the multi-thread test runtime can race the watch write ahead
        // of forward_events' first `borrow()`, letting the unfixed
        // `let _ = sender.send(m).await` see the new live sender and
        // deliver the message trivially — a polite test that doesn't guard
        // the fix.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Install the new generation's live sender — simulates install_generation.
        let (new_tx, mut new_rx) = mpsc::channel::<PresenceEvent>(1);
        watch_tx.send(new_tx).expect("watch send");

        // The fixed forward_events must re-deliver the pending event to the
        // NEW receiver. Timeout-bound so a hang becomes a clean assert.
        let received = tokio::time::timeout(Duration::from_secs(2), new_rx.recv())
            .await
            .expect("forward_events did not deliver to the new generation within 2s")
            .expect("new generation channel closed unexpectedly");

        assert_eq!(received.sensor_id, event.sensor_id);
        assert_eq!(received.state, event.state);

        cancel.cancel();
        // Drop the front sender so the spawn can exit.
        drop(front_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// Bounded-retry path: with no live generation ever arriving (a
    /// pathological non-shutdown stall), the message is dropped after
    /// `MAX_SWAP_WAITS` wait cycles and `forward_events` exits cleanly. Each
    /// `changed()` wait is resolved by writing another dead sender to the
    /// watch, so the retry loop completes and `forward_events` logs
    /// `event_forward_dropped` rather than hanging. Crucially, the front
    /// channel is NOT closed and cancellation is NOT triggered — the exit
    /// must come from the `MAX_SWAP_WAITS` bound, not from rx recv / cancel.
    #[tokio::test]
    async fn forward_events_drops_after_max_swap_waits_when_no_new_generation() {
        // Mirror the constant bound inside `deliver_or_drop`. Each write
        // lets one `changed()` resolution complete; without a live sender
        // every retry fails and the bound is hit.
        const MAX_SWAP_WAITS: usize = 2;
        let (watch_tx, watch_rx, _dead_tx) = dead_watch();
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel::<PresenceEvent>(1);

        let handle = tokio::spawn(forward_events(front_rx, watch_rx, cancel.clone()));

        let event = PresenceEvent::new(
            SensorId("doomed".into()),
            SensorState::Absent,
            Timestamp::now(),
        );
        front_tx.send(event).await.expect("front channel send");

        // Yield so forward_events processes the front message and parks on
        // its first `changed()` await before we start writing to the watch.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        for _ in 0..MAX_SWAP_WAITS {
            let (extra_dead_tx, extra_dead_rx) = mpsc::channel::<PresenceEvent>(1);
            drop(extra_dead_rx);
            watch_tx.send(extra_dead_tx).expect("watch send");
            // Yield so forward_events processes the changed() resolution,
            // re-borrow the dead sender, fail to send, and re-arm the wait.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }

        // No cancellation, front_tx still alive — forward_events must still
        // exit because `deliver_or_drop` returns false at the bound and
        // the outer loop breaks. Bound the wait so an unbounded retry
        // loop (regression) or the unfixed "swallow then await forever"
        // path becomes a clean test failure.
        let join = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("forward_events did not exit within 2s — MAX_SWAP_WAITS bound was not honored");
        assert!(join.is_ok(), "forward_events task panicked: {join:?}");
    }

    /// Cancel mid-wait: while `forward_events` is blocked on `changed()`
    /// waiting for the next generation, cancelling the token must break the
    /// inner retry loop and the outer `forward_events` cleanly — no hang, no
    /// panic.
    #[tokio::test]
    async fn forward_events_breaks_cleanly_on_cancel_during_swap_wait() {
        let (_watch_tx, watch_rx, _dead_tx) = dead_watch();
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel::<PresenceEvent>(1);

        let handle = tokio::spawn(forward_events(front_rx, watch_rx, cancel.clone()));

        // Trigger the dead-sender path so forward_events parks on changed().
        let event = PresenceEvent::new(
            SensorId("cancelled".into()),
            SensorState::Present,
            Timestamp::now(),
        );
        front_tx.send(event).await.expect("front channel send");

        // Yield so forward_events reaches its `changed()` await before we
        // cancel. Without this, on a multi-thread runtime the cancel may
        // race ahead of the task ever entering the changed() wait.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Now cancel — forward_events must exit cleanly without ever writing
        // to the watch.
        cancel.cancel();
        drop(front_tx);

        let join = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("forward_events did not exit within 1s after cancel");
        assert!(join.is_ok(), "forward_events task panicked on cancel");
    }
}

#[cfg(test)]
mod restore_tests {
    use dormant_core::fakes::RecordingSink;
    use dormant_core::rules::DisplaySnapshot;
    use dormant_core::types::BlankMode;

    use super::*;

    /// A single-display, rule-less (manual-only) engine config — mirrors
    /// `manual_cfg` in `dormant-core`'s `rules_end_to_end.rs`, but built here
    /// since `apply_restore` is private to this module.
    fn manual_engine_cfg(display: &str) -> RulesEngineConfig {
        RulesEngineConfig {
            rules: vec![],
            displays: vec![DisplayRuntimeCfg {
                display: DisplayId(display.into()),
                blank_mode: BlankMode::PowerOff,
                ladder: vec![],
                timings: DisplayRuntimeCfg::manual_defaults(Duration::from_secs(0)),
            }],
            sensors: vec![],
        }
    }

    fn display_snapshot(
        phase: &str,
        wake_attempts: u64,
        last_blank_failed: bool,
    ) -> DisplaySnapshot {
        DisplaySnapshot {
            phase: phase.into(),
            inhibited: false,
            paused: false,
            cmd_gen: 0,
            controllers: Vec::new(),
            wake_attempts,
            last_blank_failed,
            stage: None,
        }
    }

    fn snapshot_with(
        display: &str,
        phase: &str,
        wake_attempts: u64,
        last_blank_failed: bool,
    ) -> StateSnapshot {
        StateSnapshot {
            sensors: Vec::new(),
            zones: Vec::new(),
            displays: vec![(
                display.to_string(),
                display_snapshot(phase, wake_attempts, last_blank_failed),
            )],
            pending_reload: None,
        }
    }

    /// Build a fresh engine over `engine_cfg` with a `RecordingSink` for
    /// every configured display.
    fn build_engine(engine_cfg: &RulesEngineConfig) -> RulesEngine {
        let zone = ZoneEngine::new(Vec::new(), &[]).expect("empty zone engine is valid");
        let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        for dcfg in &engine_cfg.displays {
            execs.insert(dcfg.display.clone(), Arc::new(RecordingSink::new()));
        }
        RulesEngine::new(
            engine_cfg.clone(),
            zone,
            execs,
            HashMap::new(),
            Arc::new(AlwaysOwned),
        )
        .expect("valid engine config")
    }

    /// Spawn `engine.run()` and request one `StateSnapshot` over `ctl_tx`.
    /// Returns the join handle (caller cancels + awaits) and the snapshot.
    async fn spawn_and_snapshot(
        engine: RulesEngine,
        cancel: CancellationToken,
    ) -> (tokio::task::JoinHandle<()>, StateSnapshot) {
        let (events_tx, events_rx) = mpsc::channel(8);
        let (ctl_tx, ctl_rx) = mpsc::channel(8);
        let engine_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            engine.run(events_rx, ctl_rx, engine_cancel).await;
        });
        // Held so the engine's run loop (which exits when its events
        // channel closes) stays alive for the snapshot round-trip.
        let _events_tx = events_tx;

        let (tx, rx) = oneshot::channel();
        ctl_tx
            .send(ControlMsg::Snapshot(tx))
            .await
            .expect("ctl open");
        let snapshot = rx.await.expect("snapshot reply");
        (handle, snapshot)
    }

    /// `apply_restore`'s failure-state seeding (the pinned insertion point:
    /// immediately after the `dcfg` lookup, before the phase `match`) must
    /// run for EVERY display present in both the snapshot and the new
    /// engine config — independent of phase. In particular, phase "active"
    /// falls into the phase match's `_ => continue` arm; a seed call placed
    /// after that match would never run for an active display. This test is
    /// RED against that ordering mistake.
    #[tokio::test]
    async fn restore_seeds_failure_state_independent_of_phase() {
        let engine_cfg = manual_engine_cfg("mon");
        let mut engine = build_engine(&engine_cfg);

        let snap = snapshot_with("mon", "active", 5, true);
        apply_restore(&mut engine, &snap, &engine_cfg);

        let cancel = CancellationToken::new();
        let (handle, snap) = spawn_and_snapshot(engine, cancel.clone()).await;

        let d = snap
            .displays
            .iter()
            .find(|(id, _)| id == "mon")
            .expect("mon present in snapshot");
        assert_eq!(
            d.1.wake_attempts, 5,
            "seeded wake_attempts must survive phase='active'"
        );
        assert!(
            d.1.last_blank_failed,
            "seeded last_blank_failed must survive phase='active'"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    /// A display present in the restored snapshot but absent from the new
    /// engine config (dropped by the reload's config edit) must be skipped
    /// silently by `apply_restore` — no seed call, no panic.
    ///
    /// This invariant predates T3: the `dcfg`-lookup `else { continue }`
    /// guard was already there before the failure-state seeding was added.
    /// Split out from `restore_seeds_retained_display_alongside_removed`
    /// (T3-review Should-1) so this test's own RED-ness isn't conflated
    /// with the actually-new seeding behavior below.
    #[tokio::test]
    async fn restore_skips_removed_display_silently() {
        let engine_cfg = manual_engine_cfg("mon");
        let mut engine = build_engine(&engine_cfg);

        let mut snap = snapshot_with("mon", "active", 5, true);
        snap.displays
            .push(("gone".to_string(), display_snapshot("blanked", 9, true)));

        // Must not panic.
        apply_restore(&mut engine, &snap, &engine_cfg);

        let cancel = CancellationToken::new();
        let (handle, snap) = spawn_and_snapshot(engine, cancel.clone()).await;

        assert!(
            snap.displays.iter().all(|(id, _)| id != "gone"),
            "removed display must not surface in the new engine's snapshot"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    /// A display retained across reload must still have its failure state
    /// seeded (T3's actually-new behavior, see
    /// `restore_seeds_failure_state_independent_of_phase`) even when the
    /// same snapshot ALSO carries a display dropped by the reload's config
    /// edit — the removed display's `continue` must not short-circuit
    /// seeding for entries that follow it in iteration order.
    #[tokio::test]
    async fn restore_seeds_retained_display_alongside_removed() {
        let engine_cfg = manual_engine_cfg("mon");
        let mut engine = build_engine(&engine_cfg);

        let mut snap = snapshot_with("mon", "active", 5, true);
        snap.displays
            .push(("gone".to_string(), display_snapshot("blanked", 9, true)));

        apply_restore(&mut engine, &snap, &engine_cfg);

        let cancel = CancellationToken::new();
        let (handle, snap) = spawn_and_snapshot(engine, cancel.clone()).await;

        let d = snap
            .displays
            .iter()
            .find(|(id, _)| id == "mon")
            .expect("mon present in snapshot");
        assert_eq!(
            d.1.wake_attempts, 5,
            "retained display must still be seeded"
        );
        assert!(
            d.1.last_blank_failed,
            "retained display must still be seeded"
        );

        cancel.cancel();
        let _ = handle.await;
    }
}
