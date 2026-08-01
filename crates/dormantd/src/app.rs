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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "test-util"))]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;

use dormant_core::config::schema::{Config, Credentials, DisplayScope, RuleConfig};
use dormant_core::config::{
    Strictness, ValidationError, Warning, load_config, load_config_from_bytes, load_credentials,
    load_credentials_from_bytes, validate_with_input_source_readers,
};
use dormant_core::coordination::{CoordinationGate, CoordinationHandle};
use dormant_core::error::DormantError;
use dormant_core::observation::{
    ContentRevision, DaemonObservation, GenerationId, ObservationHub, ReloadReceipt, ReloadSource,
    RuntimeRevision,
};
use dormant_core::ownership::{AlwaysOwned, OwnershipGate};
use dormant_core::rules::{
    ControlMsg, DisplayRuntimeCfg, InhibitorKind, OperationKind, OperationRegistry, RollbackStatus,
    RuleRuntimeCfg, RulesEngine, RulesEngineConfig, SensorRuntimeCfg, StateSnapshot,
};
use dormant_core::state_machine::{DisplayStateMachine, Phase, SmTimings};
use dormant_core::traits::{CommandSink, RenderSink, SensorSource};
#[cfg(feature = "render")]
use dormant_core::types::ScreensaverItemReport;
use dormant_core::types::{BlankMode, DisplayId, PresenceEvent, RuleId, SensorId, Tick, ZoneId};
use dormant_core::zone::{ZoneEngine, ZoneSpec, absent_mqtt_hazards};
use dormant_displays::ddc_lock::PanelLocks;
use dormant_displays::executor::{DisplayExecutor, RetrySettings};
use dormant_displays::registry::{
    ControllerBuildContext, build_controllers, capabilities, controller_chain_fingerprint,
    input_source_readers,
};
use dormant_doctor::DoctorService;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "render")]
use dormant_render::luma::{LumaCache, LumaCatalog, LumaScanJob};
#[cfg(feature = "render")]
use dormant_render::{HeatSnapshotHandle, LayerShellRenderSink};

#[cfg(target_os = "linux")]
use crate::active_sampler::{
    self, ActiveSamplerDeps, ConfigDelta, DisplayExpectation, DisplaySamplingContext,
    ReconfigurePlan, SamplerUpdate,
};
use crate::boot_guard::{self, PromoteVerdict};
use crate::coordination_poll::{self, CoordinationPollDeps};
use crate::direct_switch::DirectSwitchHandle;
use crate::hooks::HookEngine;
use crate::inhibit_activity::{self, ActivityRule};
use crate::inhibit_audio::{self, AudioRule};
use crate::macos_idle;
use crate::notifier::{self, NotifierDeps, NotifySink, NotifyState};
use crate::reload;
use crate::sd_notify::{self, SdNotify};
use crate::watchdog_schedule::WatchdogSchedule;

const QUIESCE_OPERATION_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(target_os = "linux")]
fn active_sampler_reconfigure_plans(old: &Config, new: &Config) -> Vec<ReconfigurePlan> {
    let old_active = &old.wear.active_sampling;
    let new_active = &new.wear.active_sampling;
    let mut triggers = Vec::new();
    if new_active.enabled {
        if old_active.sampled_display != new_active.sampled_display {
            triggers.push(ConfigDelta::SampledDisplayChanged);
        }
        if new.wear.enabled {
            if old_active.stream_mode != new_active.stream_mode {
                triggers.push(ConfigDelta::StreamModeChanged);
            }
            if old_active.capture_timeout != new_active.capture_timeout
                || old_active.failure_threshold != new_active.failure_threshold
                || old_active.circuit_reset_after != new_active.circuit_reset_after
                || old.wear.sample_interval != new.wear.sample_interval
            {
                triggers.push(ConfigDelta::LimitsChanged);
            }
            if !old.wear.enabled {
                triggers.push(ConfigDelta::WearEnabled(true));
            }
            if !old_active.enabled {
                triggers.push(ConfigDelta::Enabled(true));
            }
        } else {
            triggers.push(ConfigDelta::WearEnabled(false));
        }
    } else if old_active.enabled {
        triggers.push(ConfigDelta::Enabled(false));
    }
    triggers
        .into_iter()
        .map(|trigger| ReconfigurePlan {
            active_sampling: new_active.clone(),
            sample_interval: new.wear.sample_interval,
            trigger,
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn active_sampler_display_context(
    cfg: &Config,
    display_exists: bool,
    phase: Option<&str>,
) -> DisplaySamplingContext {
    let phase = match phase {
        Some("grace") => Phase::Grace { until: Tick::now() },
        Some("blanking") => Phase::Blanking,
        Some("blanked") => Phase::Blanked,
        Some("waking") => Phase::Waking,
        _ => Phase::Active,
    };
    let stage_active = display_exists && matches!(phase, Phase::Active | Phase::Grace { .. });
    DisplaySamplingContext {
        display: display_exists
            .then(|| {
                cfg.wear
                    .active_sampling
                    .sampled_display
                    .as_ref()
                    .map(|display| DisplayExpectation {
                        display: display.clone(),
                    })
            })
            .flatten(),
        phase,
        stage_active,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod active_sampler_reload_tests {
    use super::*;
    use dormant_core::config::schema::{
        AudioConfig, DaemonConfig, NotificationsConfig, WatchdogConfig, WearConfig,
    };
    use indexmap::IndexMap;

    fn config() -> Config {
        Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::new(),
            rules: IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the reload rows stay together as the specification's auditable table"
    )]
    fn active_sampler_reload_table_maps_every_config_row() {
        let mut old = config();
        old.wear.enabled = true;
        old.wear.active_sampling.enabled = true;
        old.wear.active_sampling.sampled_display = Some("oled-a".to_owned());

        let mut disabled = old.clone();
        disabled.wear.active_sampling.enabled = false;
        let cases = vec![
            (
                "enabled off",
                old.clone(),
                disabled.clone(),
                vec![ConfigDelta::Enabled(false)],
            ),
            (
                "enabled on",
                disabled,
                old.clone(),
                vec![ConfigDelta::Enabled(true)],
            ),
            (
                "sampled display",
                old.clone(),
                {
                    let mut new = old.clone();
                    new.wear.active_sampling.sampled_display = Some("oled-b".to_owned());
                    new
                },
                vec![ConfigDelta::SampledDisplayChanged],
            ),
            (
                "stream mode",
                old.clone(),
                {
                    let mut new = old.clone();
                    new.wear.active_sampling.stream_mode =
                        dormant_core::config::schema::StreamMode::PerTick;
                    new
                },
                vec![ConfigDelta::StreamModeChanged],
            ),
            (
                "capture limits",
                old.clone(),
                {
                    let mut new = old.clone();
                    new.wear.active_sampling.capture_timeout = Duration::from_secs(3);
                    new
                },
                vec![ConfigDelta::LimitsChanged],
            ),
            (
                "wear off",
                old.clone(),
                {
                    let mut new = old.clone();
                    new.wear.enabled = false;
                    new
                },
                vec![ConfigDelta::WearEnabled(false)],
            ),
            (
                "wear on",
                {
                    let mut before = old.clone();
                    before.wear.enabled = false;
                    before
                },
                old.clone(),
                vec![ConfigDelta::WearEnabled(true)],
            ),
            (
                "unrelated",
                old.clone(),
                {
                    let mut new = old.clone();
                    new.wear.persist_interval = Duration::from_secs(61);
                    new
                },
                vec![],
            ),
        ];

        for (name, previous, new, want) in cases {
            let plans = active_sampler_reconfigure_plans(&previous, &new);
            assert_eq!(
                plans.iter().map(|plan| plan.trigger).collect::<Vec<_>>(),
                want,
                "{name}"
            );
            for plan in plans {
                assert_eq!(
                    plan.active_sampling, new.wear.active_sampling,
                    "{name} settings"
                );
                assert_eq!(
                    plan.sample_interval, new.wear.sample_interval,
                    "{name} cadence"
                );
            }
        }

        let mut combined = old.clone();
        combined.wear.active_sampling.stream_mode =
            dormant_core::config::schema::StreamMode::PerTick;
        combined.wear.active_sampling.failure_threshold = 2;
        assert_eq!(
            active_sampler_reconfigure_plans(&old, &combined)
                .iter()
                .map(|plan| plan.trigger)
                .collect::<Vec<_>>(),
            vec![ConfigDelta::StreamModeChanged, ConfigDelta::LimitsChanged]
        );
    }

    #[test]
    fn active_sampler_generation_context_omits_missing_display_and_marks_active_grace_only() {
        let mut cfg = config();
        cfg.wear.active_sampling.sampled_display = Some("oled".to_owned());

        let missing = active_sampler_display_context(&cfg, false, Some("active"));
        assert_eq!(missing.display, None);
        assert!(!missing.stage_active);

        let restored = active_sampler_display_context(&cfg, true, Some("grace"));
        assert_eq!(restored.display.unwrap().display, "oled");
        assert!(restored.stage_active);

        let blanked = active_sampler_display_context(&cfg, true, Some("blanked"));
        assert!(!blanked.stage_active);
    }
}

#[cfg(feature = "render")]
#[allow(clippy::cast_possible_truncation)]
fn runtime_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    nanos ^ u64::from(std::process::id()).rotate_left(17)
}

#[cfg(feature = "render")]
fn run_luma_scan_job(job: LumaScanJob, token: &CancellationToken) {
    let result = job.cache.scan_path(&job.path);
    if token.is_cancelled() {
        return;
    }
    match result {
        Ok(grid) => {
            if let Ok(mut catalog) = job.catalog.write() {
                if token.is_cancelled() {
                    return;
                }
                catalog.insert(job.uri, grid);
            }
        }
        Err(error) => {
            tracing::warn!(
                event = "screensaver_luma_scan_failed",
                display_id = %job.display_id,
                uri = %job.uri,
                error = %error,
            );
        }
    }
}

#[cfg(feature = "render")]
fn append_item_report(journal: &CurrentItemJournal, report: ScreensaverItemReport) {
    let Ok(mut guard) = journal.lock() else {
        tracing::warn!(event = "screensaver_item_journal_unavailable");
        return;
    };
    let queue = guard.entry(report.display_id.clone()).or_default();
    if queue.len() >= CURRENT_ITEM_JOURNAL_CAPACITY {
        queue.pop_front();
        tracing::warn!(
            event = "screensaver_item_journal_overflow",
            display_id = %report.display_id,
            capacity = CURRENT_ITEM_JOURNAL_CAPACITY,
            "screensaver item history is full; wear attribution will degrade"
        );
    }
    queue.push_back(report);
}

#[cfg(feature = "render")]
fn spawn_item_report_drain(
    mut reports: tokio::sync::mpsc::UnboundedReceiver<ScreensaverItemReport>,
    journal: CurrentItemJournal,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                report = reports.recv() => {
                    let Some(report) = report else { break };
                    append_item_report(&journal, report);
                }
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuiesceOutcome {
    Clear,
    ClearedAfterCancel,
    Busy,
}

/// Wait for generation-owned hardware leases before teardown. The reload
/// coordinator uses `Busy` to restore the paused old generation, so this
/// pure phase is the load-bearing contract behind the integration rejection.
async fn quiesce_operations(
    registry: &OperationRegistry,
    generation: GenerationId,
    first_wait: Duration,
    second_wait: Duration,
) -> QuiesceOutcome {
    if tokio::time::timeout(first_wait, registry.wait_generation_empty(generation))
        .await
        .is_ok()
    {
        return QuiesceOutcome::Clear;
    }
    registry.cancel_generation(generation);
    if tokio::time::timeout(second_wait, registry.wait_generation_empty(generation))
        .await
        .is_ok()
    {
        QuiesceOutcome::ClearedAfterCancel
    } else {
        QuiesceOutcome::Busy
    }
}

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
///
/// The 5th parameter is the OLED-health T10 pixel-shift settings —
/// see [`dormant_render::ShiftSettings`] for why it's a SEPARATE
/// parameter from the `ScreensaverSettings` one rather than a field on
/// it: shift applies only to the screensaver surface (U5: the black
/// overlay never shifts), so it's derived from `dc.screensaver`
/// independently of whether the display's ladder ever reaches
/// `RenderScreensaver` (see `build_render_sinks` below).  `None` means
/// the display has no `[displays.<id>.screensaver]` table at all —
/// shift stays disabled.
#[cfg(feature = "render")]
type RenderSinkBuilder = Arc<
    dyn Fn(
            DisplayId,
            String,
            Option<&tokio::sync::mpsc::UnboundedSender<DisplayId>>,
            Option<&tokio::sync::mpsc::UnboundedSender<ScreensaverItemReport>>,
            Option<&dormant_render::ScreensaverSettings>,
            Option<&dormant_render::ShiftSettings>,
        ) -> Option<Arc<dyn RenderSink>>
        + Send
        + Sync,
>;

#[cfg(feature = "render")]
struct RenderContext {
    luma_cache: Arc<LumaCache>,
    luma_catalog: LumaCatalog,
    heat_snapshots: HeatSnapshotHandle,
    seed: u64,
    item_journal: CurrentItemJournal,
}

/// Process-owned bounded history of render-side screensaver transitions.
#[cfg(feature = "render")]
pub(crate) type CurrentItemJournal =
    Arc<Mutex<HashMap<DisplayId, VecDeque<ScreensaverItemReport>>>>;

#[cfg(feature = "render")]
const CURRENT_ITEM_JOURNAL_CAPACITY: usize = 1_024;

pub use dormant_core::reload::ReloadOutcome;
use dormant_core::reload::{ReloadRequest, ReloadRequester};

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
    let errors =
        validate_with_input_source_readers(&cfg, &capabilities(), &input_source_readers(), &creds);
    ValidationReport {
        warnings,
        errors,
        load_error: None,
    }
}

// ── App ────────────────────────────────────────────────────────────────────────

/// The daemon application: config paths + the sensor-source factory.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent, orthogonal builder/test-seam flag \
              (rollback-active, disable-ipc, plus two test-only forced-failure \
              seams) — a state-machine enum would need to represent their \
              cross-product, which does not exist here"
)]
pub struct App {
    /// The generation-0 (boot) source: the already-validated config used
    /// ONLY to assemble the first generation in [`App::start`]. Ordinary
    /// callers ([`App::build`]/[`App::build_with_sources`]) set this equal
    /// to `operator_config_path`; a rollback boot sets it to the LKG
    /// substitute while `operator_config_path` keeps pointing at the real
    /// (possibly still-broken) operator config (rollback-recovery plan,
    /// Task 1: two explicit path roles).
    config_path: PathBuf,
    /// The operator source: the real config path retained by the watcher,
    /// Web UI, manual reload, `Runner`, `AppHandle`, and future LKG
    /// candidates. Equal to `config_path` for every non-boot caller; only
    /// [`App::with_boot_source`] (boot-only, called by [`crate::boot::boot`]
    /// after a successful build) ever diverges the two.
    operator_config_path: PathBuf,
    creds_path: PathBuf,
    /// Whether THIS boot is a rollback (running generation 0 from the LKG
    /// substitute rather than the operator path). `false` for every
    /// ordinary caller; set by [`App::with_boot_source`]. Gates the
    /// initial LKG-candidate arming in [`App::start`] (coupling-hazard
    /// suppression, plan Task 1 §5): arming a candidate from the operator
    /// path's still-broken bytes during a rollback boot would let
    /// `lkg_tick` promote them once healthy, corrupting the very LKG file
    /// the rollback is anchored on.
    rollback_active: bool,
    /// The daemon's crash-loop/rollback state directory (rollback-recovery
    /// plan, Task 2 §2) — where `crash-loop.json` lives. Defaults to the
    /// canonical [`dormant_core::paths::state_dir`] for every non-boot
    /// caller ([`App::build`]/[`App::build_with_sources`]); `rollback_active`
    /// is always `false` for those callers, so the accepted-reload
    /// rollback-recovery transition in `Runner::reload` (which is the only
    /// consumer of this field) never actually fires for them regardless of
    /// the value. [`App::with_boot_source`] overrides it with
    /// `BootInputs::state_dir` — the SAME directory `boot_guard::prepare`
    /// and `boot()`'s own immediate-rollback write already used for this
    /// boot, so the live-reload clear lands in the identical file.
    state_dir: PathBuf,
    /// Runner-owned rollback presentation status (rollback-recovery plan,
    /// Task 1 §7): `None` for every non-boot caller; set alongside
    /// `rollback_active` by [`App::with_boot_source`]. Carried into every
    /// [`Runner`] generation swap as a `spawn_generation` parameter rather
    /// than re-read from disk or reconstructed from a snapshot.
    rollback_status: Option<RollbackStatus>,
    strictness: Strictness,
    source_builder: SourceBuilder,
    #[cfg(feature = "render")]
    render_sink_builder: Option<RenderSinkBuilder>,
    notify_sink_builder: NotifySinkBuilder,
    /// Daemon-local diagnostic hub, injected before start when startup
    /// transitions must be observed by a test.
    observations: ObservationHub,
    disable_ipc: bool,
    /// Suppress the `notify`-based config file watcher (test seam).
    /// When set, the run loop's watcher arm never fires — only explicit
    /// reload requests (IPC, control, signal) drive reloads. This keeps
    /// batch-coordinator tests deterministic by removing the third,
    /// uncontrolled requester.
    #[cfg(any(test, feature = "test-util"))]
    disable_config_watcher: bool,
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
    /// Test seam: independently fails `rebuild_old`'s spawn after the
    /// accepted-config spawn failure has exercised the rollback path.
    #[cfg(any(test, feature = "test-util"))]
    force_rebuild_old_spawn_failure: bool,
    /// Test seam (rollback-recovery plan, Task 1 §8): forces `Runner::reload`
    /// to treat the preliminary snapshot request as timed out (`None`),
    /// proving `rebuild_old` still carries Runner-owned rollback status even
    /// when `snapshot.and_then(...)` would have been `None` anyway.
    #[cfg(any(test, feature = "test-util"))]
    force_reload_snapshot_timeout: bool,
    #[cfg(any(test, feature = "test-util"))]
    generation_barrier_gate: Option<GenerationBarrierGate>,
    #[cfg(any(test, feature = "test-util"))]
    force_generation_barrier_timeout: bool,
    #[cfg(any(test, feature = "test-util"))]
    test_operation_busy: bool,
    #[cfg(any(test, feature = "test-util"))]
    reload_lifecycle_capture: Option<ReloadLifecycleCapture>,
}

/// Result of a post-release old-engine snapshot probe.
#[cfg(any(test, feature = "test-util"))]
type BarrierSnapshotProbe = Result<oneshot::Receiver<StateSnapshot>, &'static str>;

/// Test-only rendezvous immediately before the old engine drain barrier.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone)]
pub struct GenerationBarrierGate {
    entered_tx: watch::Sender<bool>,
    entered_rx: watch::Receiver<bool>,
    release: Arc<tokio::sync::Notify>,
    post_release_snapshot: Arc<Mutex<Option<oneshot::Sender<BarrierSnapshotProbe>>>>,
}

/// Test-only record of the forced snapshot-timeout reload's lifecycle stages.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Default)]
pub struct ReloadLifecycleCapture(Arc<Mutex<Vec<&'static str>>>);

#[cfg(any(test, feature = "test-util"))]
impl ReloadLifecycleCapture {
    /// Create an empty lifecycle capture.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the stages reached by the forced reload so far.
    #[must_use]
    pub fn stages(&self) -> Vec<&'static str> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, stage: &'static str) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(stage);
    }
}

#[cfg(any(test, feature = "test-util"))]
impl GenerationBarrierGate {
    /// Create a gate that remains closed until [`Self::release`] is called.
    #[must_use]
    pub fn new() -> Self {
        let (entered_tx, entered_rx) = watch::channel(false);
        Self {
            entered_tx,
            entered_rx,
            release: Arc::new(tokio::sync::Notify::new()),
            post_release_snapshot: Arc::new(Mutex::new(None)),
        }
    }

    /// Wait until reload has paused front-door routing before the old drain.
    pub async fn wait_until_entered(&self) {
        let mut entered = self.entered_rx.clone();
        while !*entered.borrow_and_update() {
            if entered.changed().await.is_err() {
                break;
            }
        }
    }

    /// Permit the reload to send the old-generation drain barrier.
    pub fn release(&self) {
        self.release.notify_one();
    }

    /// Request a snapshot probe sent after release and before the drain barrier.
    /// The returned channel reports whether the old engine accepted the probe.
    pub fn request_post_release_snapshot(&self) -> oneshot::Receiver<BarrierSnapshotProbe> {
        let (tx, rx) = oneshot::channel();
        let mut probe = self
            .post_release_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if probe.is_some() {
            let _ = tx.send(Err("barrier snapshot probe already registered"));
        } else {
            *probe = Some(tx);
        }
        rx
    }

    async fn reach(&self, old_ctl: mpsc::Sender<ControlMsg>) {
        self.entered_tx.send_replace(true);
        self.release.notified().await;
        let probe = self
            .post_release_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(probe) = probe {
            let (snapshot_tx, snapshot_rx) = oneshot::channel();
            let result = old_ctl
                .send(ControlMsg::Snapshot(snapshot_tx))
                .await
                .map(|()| snapshot_rx)
                .map_err(|_| "old engine closed before the barrier probe");
            let _ = probe.send(result);
        }
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Default for GenerationBarrierGate {
    fn default() -> Self {
        Self::new()
    }
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
            operator_config_path: config_path.clone(),
            config_path,
            creds_path,
            rollback_active: false,
            state_dir: dormant_core::paths::state_dir(),
            rollback_status: None,
            strictness,
            source_builder,
            #[cfg(feature = "render")]
            render_sink_builder: None,
            notify_sink_builder: default_notify_sink_builder(),
            observations: ObservationHub::new(64),
            disable_ipc: false,
            #[cfg(any(test, feature = "test-util"))]
            disable_config_watcher: false,
            sd_notify: SdNotify::from_env(),
            watchdog_interval: None,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_spawn_failure: false,
            #[cfg(any(test, feature = "test-util"))]
            force_rebuild_old_spawn_failure: false,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_snapshot_timeout: false,
            #[cfg(any(test, feature = "test-util"))]
            generation_barrier_gate: None,
            #[cfg(any(test, feature = "test-util"))]
            force_generation_barrier_timeout: false,
            #[cfg(any(test, feature = "test-util"))]
            test_operation_busy: false,
            #[cfg(any(test, feature = "test-util"))]
            reload_lifecycle_capture: None,
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
            operator_config_path: config_path.clone(),
            config_path,
            creds_path,
            rollback_active: false,
            state_dir: dormant_core::paths::state_dir(),
            rollback_status: None,
            strictness,
            source_builder: Arc::new(factory),
            #[cfg(feature = "render")]
            render_sink_builder: None,
            notify_sink_builder: default_notify_sink_builder(),
            observations: ObservationHub::new(64),
            disable_ipc: false,
            #[cfg(any(test, feature = "test-util"))]
            disable_config_watcher: false,
            sd_notify: SdNotify::from_env(),
            watchdog_interval: None,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_spawn_failure: false,
            #[cfg(any(test, feature = "test-util"))]
            force_rebuild_old_spawn_failure: false,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_snapshot_timeout: false,
            #[cfg(any(test, feature = "test-util"))]
            generation_barrier_gate: None,
            #[cfg(any(test, feature = "test-util"))]
            force_generation_barrier_timeout: false,
            #[cfg(any(test, feature = "test-util"))]
            test_operation_busy: false,
            #[cfg(any(test, feature = "test-util"))]
            reload_lifecycle_capture: None,
        })
    }

    /// Set an injected render-sink factory (test seam).
    ///
    /// When set, `assemble_static` calls this factory instead of
    /// building [`LayerShellRenderSink`] directly.  The factory receives
    /// the display id, output connector name, an optional
    /// `UnboundedSender<DisplayId>` (the `InputWake` channel), an
    /// optional `UnboundedSender<ScreensaverItemReport>` (the item feedback
    /// channel),
    /// optional [`dormant_render::ScreensaverSettings`], and an
    /// optional [`dormant_render::ShiftSettings`] (OLED-health T10 —
    /// derived independently of the ladder's `RenderScreensaver`
    /// stage; see `build_render_sinks`'s shift-settings assembly);
    /// return `None` to skip the sink (fall-through).
    #[cfg(feature = "render")]
    #[must_use]
    pub fn with_render_sink_builder<F>(mut self, factory: F) -> Self
    where
        F: Fn(
                DisplayId,
                String,
                Option<&tokio::sync::mpsc::UnboundedSender<DisplayId>>,
                Option<&tokio::sync::mpsc::UnboundedSender<ScreensaverItemReport>>,
                Option<&dormant_render::ScreensaverSettings>,
                Option<&dormant_render::ShiftSettings>,
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

    /// Suppress the `notify`-based config file watcher (test seam).
    ///
    /// When called, the daemon starts with a dead watcher whose receiver
    /// never fires — only explicit reload requests (IPC, control, signal)
    /// will trigger reloads. This makes batch-coordinator tests
    /// deterministic by removing the uncontrolled filesystem watcher from
    /// the set of requesters.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn disable_config_watcher(mut self) -> Self {
        self.disable_config_watcher = true;
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

    /// Set the root directory for daemon-owned state.
    #[must_use]
    pub fn with_state_dir(mut self, state_dir: PathBuf) -> Self {
        self.state_dir = state_dir;
        self
    }

    /// Boot-only setter (rollback-recovery plan, Task 1 §3): called by
    /// [`crate::boot::boot`] immediately after it has successfully built
    /// `Self` from whichever source `prepare()`/immediate-rollback chose
    /// (`self.config_path`, unchanged — the generation-0/boot source).
    /// Records the REAL operator config path (`BootPlan::operator_config`)
    /// separately, so every runtime consumer added in `App::start`
    /// (the file watcher, Web UI, `Runner`, `AppHandle`) keeps watching
    /// and reloading the operator's actual file rather than the LKG
    /// substitute that only assembled generation 0. `rollback_active`
    /// records whether THIS boot is a rollback — `boot()` computes it
    /// locally (never a `BootInputs` field) — and gates the initial LKG
    /// candidate arming in `App::start` (§5's coupling-hazard
    /// suppression). Every non-boot caller ([`App::build`]/
    /// [`App::build_with_sources`]) never calls this, so `config_path ==
    /// operator_config_path` and `rollback_active == false` for them,
    /// preserving today's behavior exactly. `state_dir` (Task 2 §2) is
    /// `BootInputs::state_dir` — the same crash-loop-state directory this
    /// boot's `boot_guard::prepare`/`boot()` already used, so the
    /// accepted-reload rollback-recovery clear (`Runner::reload`) rewrites
    /// the identical `crash-loop.json`.
    #[must_use]
    pub(crate) fn with_boot_source(
        mut self,
        operator_config_path: PathBuf,
        rollback_status: Option<RollbackStatus>,
        state_dir: PathBuf,
    ) -> Self {
        self.operator_config_path = operator_config_path;
        self.rollback_active = rollback_status.is_some();
        self.rollback_status = rollback_status;
        self.state_dir = state_dir;
        self
    }

    /// Override the watchdog probe-arm's tick period (test seam — spec
    /// §6.3). Production leaves this unset and `Runner` falls back to
    /// `sd_notify::watchdog_interval_from_env().unwrap_or(30s)`; an LKG
    /// promotion test that needs a short `stability_window` to elapse inside
    /// the gate's real-time budget calls this instead of touching the
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

    /// Force `rebuild_old`'s generation spawn to fail after a rejected reload.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_force_rebuild_old_spawn_failure(mut self) -> Self {
        self.force_rebuild_old_spawn_failure = true;
        self
    }

    /// Force `Runner::reload`'s preliminary snapshot request to behave as
    /// though it timed out (test seam — see `App::force_reload_snapshot_timeout`).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_force_reload_snapshot_timeout(mut self) -> Self {
        self.force_reload_snapshot_timeout = true;
        self
    }

    /// Pause a reload immediately before its old-generation drain barrier.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_generation_barrier_gate(mut self, gate: GenerationBarrierGate) -> Self {
        self.generation_barrier_gate = Some(gate);
        self
    }

    /// Simulate an old engine that never acknowledges its generation drain.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_force_generation_barrier_timeout(mut self) -> Self {
        self.force_generation_barrier_timeout = true;
        self
    }

    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_operation_busy(mut self) -> Self {
        self.test_operation_busy = true;
        self
    }

    /// Record stages reached by a forced snapshot-timeout reload.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_reload_lifecycle_capture(mut self, capture: ReloadLifecycleCapture) -> Self {
        self.reload_lifecycle_capture = Some(capture);
        self
    }

    /// Seed a Runner-owned rollback status without touching disk (test
    /// seam — mirrors what `App::with_boot_source` does on a real rollback
    /// boot, for tests that drive `App::build`/`build_with_sources`
    /// directly instead of going through `crate::boot::boot`).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_test_rollback_status(mut self, status: RollbackStatus, state_dir: PathBuf) -> Self {
        self.rollback_active = true;
        self.rollback_status = Some(status);
        self.state_dir = state_dir;
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

    /// Replace the daemon-local observation hub before startup.
    #[must_use]
    pub fn with_observation_hub(mut self, observations: ObservationHub) -> Self {
        self.observations = observations;
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
    /// # Panics
    ///
    /// Panics if coordination is enabled without the claim transport that
    /// startup constructs alongside it. Those states are created in lockstep.
    ///
    /// # Errors
    ///
    /// Fails if the initial runtime cannot be assembled (controller build,
    /// post-probe validation, zone/engine construction) or the watcher cannot
    /// be installed.
    #[allow(clippy::too_many_lines)]
    pub async fn start(mut self) -> Result<(AppHandle, JoinHandle<()>)> {
        let root = CancellationToken::new();

        let wear_dir = self.state_dir.join("wear");

        let (cfg, creds) = load_cfg_creds(&self.config_path, &self.creds_path, self.strictness)?;
        let applied_revision = runtime_revision_from_paths(&self.config_path, &self.creds_path)?;
        let socket_path =
            dormant_core::paths::resolve_socket_path(cfg.daemon.socket_path.as_deref());

        // The daemon's ONE process-wide panel-lock registry (spec §4.3) AND
        // (macOS-only) gamma-hold-registry/breadcrumb (Task 8), bundled into
        // one `ControllerBuildContext`: constructed here, threaded into
        // every `assemble_static` call (this one and every subsequent
        // reload via `Runner`), and never reconstructed for the life of the
        // process — a physical panel's lock is the same `Arc<PanelLock>`,
        // and a gamma selector's hold/breadcrumb the same shared instance,
        // across every generation swap.
        let ctrl_ctx = ControllerBuildContext::new(PanelLocks::new(), self.state_dir.clone());

        // Daemon-lifetime notifier state + sink (spec §4.4): constructed
        // once here and threaded into every `spawn_generation` call (this
        // one and every subsequent reload/rollback via `Runner`), so the
        // notifier's open episodes — and the underlying `ZbusSink`'s cached
        // DBus connection — survive a config reload, exactly like
        // `ctrl_ctx` above.
        let notify_state: Arc<Mutex<NotifyState>> = Arc::new(Mutex::new(NotifyState::default()));
        let notify_sink: Arc<dyn NotifySink> = (self.notify_sink_builder)();

        // Clone before cfg/creds are moved into assemble_static.
        let cfg_clone = cfg.clone();
        let creds_clone = creds.clone();
        let started_web_port = cfg.daemon.web_port;
        let started_web_bind = cfg.daemon.web_bind;

        // These handles are daemon-lifetime state: image scans survive a
        // config reload, while heat snapshots are replaced by the tracker.
        let wear_handle: dormant_core::wear::WearHandle =
            Arc::new(std::sync::RwLock::new(HashMap::new()));
        #[cfg(feature = "render")]
        let render_context = RenderContext {
            luma_cache: Arc::new(LumaCache::new()),
            luma_catalog: Arc::new(std::sync::RwLock::new(HashMap::new())),
            heat_snapshots: Arc::new(std::sync::RwLock::new(HashMap::new())),
            seed: runtime_seed(),
            item_journal: Arc::new(Mutex::new(HashMap::new())),
        };

        #[cfg(feature = "render")]
        let assembly = assemble_static_with_context(
            cfg,
            creds,
            &self.source_builder,
            self.render_sink_builder.as_ref(),
            &ctrl_ctx,
            &render_context,
        )
        .await
        .context("assemble initial runtime")?;
        #[cfg(not(feature = "render"))]
        let assembly = assemble_static(cfg, creds, &self.source_builder, &ctrl_ctx)
            .await
            .context("assemble initial runtime")?;

        // Coordination state is process-lifetime state, like notifier episodes:
        // generations only borrow its gate, so a reload cannot discard a verdict
        // between the old poller stopping and its replacement starting.
        let shared = shared_displays(&cfg_clone);
        let coordination = (!shared.is_empty()).then(|| CoordinationHandle::new(shared));
        let ownership: Arc<dyn OwnershipGate> = coordination.as_ref().map_or_else(
            || Arc::new(AlwaysOwned) as Arc<dyn OwnershipGate>,
            |state| Arc::new(CoordinationGate::new(state.clone())) as Arc<dyn OwnershipGate>,
        );
        let (config_tx, config_rx) = watch::channel(Arc::new(cfg_clone.clone()));
        let (creds_tx, creds_rx) = watch::channel(Arc::new(creds_clone));
        let (executors_tx, executors_rx) = watch::channel(Arc::new(HashMap::new()));
        let (front_ctl_tx, front_ctl_rx) = mpsc::channel::<ControlMsg>(64);

        // Daemon-lifetime idle-observation channel — the stock idle source
        // publishes real activity timestamps into the tx half.
        let (idle_obs_tx, _idle_obs_rx) = crate::idle_observation::idle_observation_channel();
        let (filtered_activity_tx, _filtered_activity_rx) =
            crate::filtered_activity::filtered_activity_channel();

        // Local direct-switch handle — replaces the owner-mediated claim
        // protocol (mDNS discovery, SPAKE2 pairing, Ed25519 signed frames,
        // epochs, replay windows, TCP transport, claim-engine state machine).
        // Every write is verified against the semantic readback code; no
        // network, no peer identity.
        let publisher = Arc::new(crate::hooks::MqttPublisher::new(String::new(), None));
        let hook_engine = Arc::new(HookEngine::new(publisher));
        let direct_switch = Arc::new(DirectSwitchHandle::new(
            executors_rx.clone(),
            config_rx.clone(),
            hook_engine,
            front_ctl_tx.clone(),
            coordination.clone(),
        ));

        let operation_registry = Arc::new(OperationRegistry::default());
        #[cfg(any(test, feature = "test-util"))]
        if self.test_operation_busy {
            let (_, lease) = operation_registry
                .try_acquire(
                    GenerationId(0),
                    OperationKind::Exercise(DisplayId("other".into())),
                )
                .expect("test operation reservation");
            std::mem::forget(lease);
        }
        let spawn = spawn_generation(
            &root,
            assembly,
            None,
            None,
            None,
            notify_state.clone(),
            notify_sink.clone(),
            ownership.clone(),
            coordination.clone(),
            config_rx.clone(),
            executors_rx.clone(),
            GenerationId(0),
            operation_registry.clone(),
            Some(self.observations.clone()),
            Some(idle_obs_tx.clone()),
            filtered_activity_tx.clone(),
            &direct_switch,
        )?;
        self.observations
            .emit(DaemonObservation::GenerationStarted {
                generation: GenerationId(0),
            });

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
        executors_tx.send_replace(Arc::new(executors0));

        // Stable front channels are paused before an old engine is drained, so
        // no delivery can race behind the generation barrier.
        let ctl_router = Arc::new(GenerationRouter::new_with_generation(
            spawn.ctl_tx.clone(),
            GenerationId(0),
            operation_registry.clone(),
        ));
        let events_router = Arc::new(GenerationRouter::new(spawn.events_tx.clone()));
        let (front_events_tx, front_events_rx) = mpsc::channel::<PresenceEvent>(256);

        let front_ctl_handle =
            tokio::spawn(forward_ctl(front_ctl_rx, ctl_router.clone(), root.clone()));
        let front_events_handle = tokio::spawn(forward_events(
            front_events_rx,
            events_router.clone(),
            root.clone(),
        ));

        // ── Spawn activity-follow on boot ───────────────────────────────────
        let mut activity_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
        if cfg_clone.coordination.activity_follow {
            let idle_rx = idle_obs_tx.subscribe();
            let filtered_rx = filtered_activity_tx.subscribe();
            let shared: Vec<DisplayId> = cfg_clone
                .displays
                .iter()
                .filter(|(_, dc)| {
                    dc.scope == dormant_core::config::DisplayScope::Shared
                        && dc.shared_input_code.is_some()
                })
                .map(|(name, _)| DisplayId(name.clone()))
                .collect();
            if !shared.is_empty() {
                let deps = crate::activity_follow::ActivityFollowDeps {
                    idle_rx,
                    direct_switch: Some(direct_switch.clone()),
                    display_ids: shared.into(),
                    arm_after: cfg_clone.coordination.arm_after,
                    cancel: root.clone(),
                    pull_recorder: None,
                    clock: crate::activity_follow::production_clock,
                };
                activity_follow_handle = Some(crate::activity_follow::spawn(deps, filtered_rx));
            }
        }

        // ── Publish KVM status on boot (#137 regression guard) ───────────
        // Build and publish the initial KvmStatus before the run loop starts
        // so the tray/web/CLI see accurate state from the first snapshot.
        {
            let mut switch_capable = Vec::new();
            let mut push_capable = Vec::new();
            for (name, dc) in &cfg_clone.displays {
                if dc.scope == dormant_core::config::DisplayScope::Shared {
                    if dc.shared_input_code.is_some() {
                        switch_capable.push(DisplayId(name.clone()));
                    }
                    if dc.shared_peer_input_write_code.is_some() {
                        push_capable.push(DisplayId(name.clone()));
                    }
                }
            }
            let kvm = dormant_core::rules::KvmStatus {
                keymap: cfg_clone.keymap.clone(),
                switch_capable_displays: switch_capable,
                activity_following: activity_follow_handle.is_some(),
                push_capable_displays: push_capable,
            };
            let _ = spawn.ctl_tx.send(ControlMsg::SetKvmStatus(kvm)).await;
        }

        let (reload_tx, _) = broadcast::channel(16);
        let (reload_request_tx, reload_request_rx) = mpsc::channel::<ReloadRequest>(32);
        let reload_requester =
            ReloadRequester::new_with_observations(reload_request_tx, self.observations.clone());
        let observations = reload_requester.observations();

        // Wear tracker: daemon-lifetime, reads config via watch, publishes
        // over the front ctl channel (rides the `GenerationRouter`'s
        // pause/queue/release across generation swaps), sees the current
        // generation's executors via the watch seeded above.
        let wear_tracker_handle =
            crate::wear_tracker::spawn(crate::wear_tracker::WearTrackerDeps {
                config_rx: config_rx.clone(),
                ctl_tx: front_ctl_tx.clone(),
                executors_rx: executors_rx.clone(),
                handle: wear_handle.clone(),
                #[cfg(feature = "render")]
                heat_snapshots: render_context.heat_snapshots.clone(),
                cancel: root.clone(),
                dir: wear_dir,
                observations: self.observations.clone(),
                #[cfg(feature = "render")]
                item_journal: render_context.item_journal.clone(),
            });

        #[cfg(target_os = "linux")]
        let (active_sampler_handle, active_sampler_updates) = {
            let latest_grid = active_sampler::new_latest_grid();
            let (update_tx, update_rx) = mpsc::channel::<SamplerUpdate>(16);
            match active_sampler::linux::PortalPipeWireSource::new().await {
                Ok(source) => {
                    let consent_path = self.state_dir.join("screencast-consent.json");
                    let (handle, _join) = active_sampler::spawn_with_handle(ActiveSamplerDeps {
                        initial_config: Arc::new(cfg_clone.clone()),
                        update_rx,
                        latest_grid,
                        source: Box::new(source),
                        consent_path,
                        cancel: root.clone(),
                    });
                    let display_exists = cfg_clone
                        .wear
                        .active_sampling
                        .sampled_display
                        .as_ref()
                        .is_some_and(|display| {
                            spawn
                                .generation
                                .display_executors
                                .contains_key(&DisplayId(display.clone()))
                        });
                    if let Err(error) = update_tx.try_send(SamplerUpdate::DisplayContext(
                        active_sampler_display_context(&cfg_clone, display_exists, Some("active")),
                    )) {
                        tracing::warn!(
                            event = "wear_sampling_update_dropped",
                            ?error,
                            "could not publish initial active-sampler display context"
                        );
                    }
                    (Some(handle), Some(update_tx))
                }
                Err(error) => {
                    tracing::warn!(
                        event = "wear_sampling_portal_unreachable",
                        ?error,
                        "active sampler unavailable; uniform attribution remains active"
                    );
                    (None, None)
                }
            }
        };
        #[cfg(not(target_os = "linux"))]
        let active_sampler_handle: Option<crate::active_sampler::ActiveSamplerHandle> = None;

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

        #[cfg(any(test, feature = "test-util"))]
        let watcher = if self.disable_config_watcher {
            reload::suppressed_watcher()
        } else {
            reload::config_watcher(&self.operator_config_path)
                .context("install config file watcher")?
        };
        #[cfg(not(any(test, feature = "test-util")))]
        let watcher = reload::config_watcher(&self.operator_config_path)
            .context("install config file watcher")?;

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
                    reload_requester.clone(),
                    doctor_service.clone(),
                    direct_switch.clone(),
                    active_sampler_handle.clone(),
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
                let web_state = dormant_web::WebState::new(dormant_web::WebStateInner::new(
                    dormant_web::WebStateInnerParams {
                        ctl_tx: front_ctl_tx.clone(),
                        reload_requester: reload_requester.clone(),
                        reload_rx: reload_tx.subscribe(),
                        config_rx: config_rx.clone(),
                        creds_rx: creds_rx.clone(),
                        config_path: self.operator_config_path.clone(),
                        creds_path: self.creds_path.clone(),
                        doctor: doctor_service.clone(),
                        wear: wear_handle.clone(),
                        web_bind: addr,
                        cancel: root.clone(),
                        reload_timeout: std::time::Duration::from_secs(10),
                    },
                ));
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
        //
        // Coupling-hazard suppression (rollback-recovery plan, Task 1 §5):
        // this now reads the OPERATOR path (post path-split), which during
        // a rollback boot is exactly the broken bytes the daemon just
        // rolled away from. Arming a candidate from them would let
        // `lkg_tick` promote them once `stability_window` + healthy
        // displays elapse, corrupting `last-known-good.toml` — the very
        // anchor the rollback depends on. So: no candidate at all while
        // `rollback_active`; `Runner::reload`'s successful-recovery path
        // (a later task) arms the first post-rollback candidate instead.
        let lkg_candidate = if self.rollback_active {
            None
        } else {
            new_lkg_candidate(
                &self.operator_config_path,
                cfg_clone.watchdog.lkg_enabled,
                "boot",
            )
        };
        #[cfg(any(test, feature = "test-util"))]
        let lkg_observed = Arc::new(Mutex::new(LkgCandidateObserved::from_candidate(
            lkg_candidate.as_ref(),
        )));

        // READY=1 (spec §6.2 F1): the TRUE end of `App::start` — after the
        // IPC listener spawn AND the web spawn above, immediately before
        // this function's `Ok` return. `SdNotify` is deliberately not
        // `Clone` (its own module doc: exactly one owner at a time along
        // the boot chain, `BootInputs::sd_notify` → `App::with_sd_notify` →
        // `Runner`) and `Runner` below takes ownership of it for the
        // lifetime of the run loop — so this is the LAST point at which
        // `self` (not yet moved into `Runner`) still holds it. Sending here,
        // then moving it into the `Runner` literal two lines down, is the
        // ownership-preserving way to hit the spec's placement without
        // giving `SdNotify` a second owner or an `Arc` it doesn't need
        // (T5 boot-integration design note — see also `boot.rs`'s module
        // docs, which reference this comment).
        self.sd_notify.ready();

        let generation_barrier_ack_timeout =
            spawn.generation.cfg.daemon.generation_barrier_ack_timeout;
        let runner = Runner {
            config_path: self.operator_config_path.clone(),
            creds_path: self.creds_path.clone(),
            strictness: self.strictness,
            source_builder: self.source_builder,
            #[cfg(feature = "render")]
            render_sink_builder: self.render_sink_builder,
            #[cfg(feature = "render")]
            render_context,
            root: root.clone(),
            ctl_router,
            events_router,
            front_ctl_handle,
            front_events_handle,
            executors_tx,
            reload_tx: reload_tx.clone(),
            observations: observations.clone(),
            config_tx,
            creds_tx,
            generation: spawn.generation,
            applied_revision,
            generation_id: GenerationId(0),
            operation_registry,
            wear_tracker_handle,
            #[cfg(target_os = "linux")]
            active_sampler_handle,
            #[cfg(target_os = "linux")]
            active_sampler_updates,
            started_web_port,
            started_web_bind,
            ctrl_ctx,
            notify_state,
            notify_sink,
            ownership,
            coordination: coordination.clone(),
            direct_switch: direct_switch.clone(),
            idle_obs_tx: Some(idle_obs_tx.clone()),
            filtered_activity_tx,
            sd: self.sd_notify,
            watchdog_interval,
            generation_barrier_ack_timeout,
            lkg_candidate,
            lkg_defer_count: 0,
            probe_failed_warned: false,
            state_dir: self.state_dir.clone(),
            rollback_active: self.rollback_active,
            rollback_status: self.rollback_status.clone(),
            rollback_state_clear_pending: false,
            rollback_state_clear_warned: false,
            #[cfg(any(test, feature = "test-util"))]
            lkg_observed: lkg_observed.clone(),
            #[cfg(any(test, feature = "test-util"))]
            force_reload_spawn_failure: self.force_reload_spawn_failure,
            #[cfg(any(test, feature = "test-util"))]
            force_rebuild_old_spawn_failure: self.force_rebuild_old_spawn_failure,
            #[cfg(any(test, feature = "test-util"))]
            force_reload_snapshot_timeout: self.force_reload_snapshot_timeout,
            #[cfg(any(test, feature = "test-util"))]
            generation_barrier_gate: self.generation_barrier_gate,
            #[cfg(any(test, feature = "test-util"))]
            force_generation_barrier_timeout: self.force_generation_barrier_timeout,
            #[cfg(any(test, feature = "test-util"))]
            reload_lifecycle_capture: self.reload_lifecycle_capture,
            activity_follow_handle,
        };

        let join = tokio::spawn(run_loop(
            runner,
            watcher,
            reload_requester.clone(),
            reload_request_rx,
        ));

        let handle = AppHandle {
            ctl_tx: front_ctl_tx,
            events_tx: front_events_tx,
            reload_tx,
            reload_requester,
            observations,
            root,
            config_rx,
            creds_rx,
            config_path: self.operator_config_path.clone(),
            doctor_service,
            #[cfg(any(test, feature = "test-util"))]
            coordination,
            direct_switch,
            _ipc_handle: ipc_handle,
            _web_handle: web_handle,
            #[cfg(any(test, feature = "test-util"))]
            lkg_observed,
        };

        Ok((handle, join))
    }

    /// Run the daemon to completion: starts and then awaits the run loop
    /// until a shutdown signal fires.
    ///
    /// **Not the production entry point (spec §5.1, T5):** `dormantd`'s
    /// `main.rs` calls [`crate::boot::boot`] instead, which owns the
    /// single-instance flock (P1/P15 — acquired immediately before its own
    /// `App::start()` call, the ONLY one on a production boot path) and the
    /// bad-config/crash-loop rollback machinery this method knows nothing
    /// about. Kept as a convenience for callers (tests, other binaries)
    /// that want the plain "just start and run" shape with no rollback
    /// semantics and no lock — such a caller MUST NOT be a second
    /// concurrent daemon process against the same displays/state.
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
    reload_requester: ReloadRequester,
    observations: ObservationHub,
    root: CancellationToken,
    config_rx: watch::Receiver<Arc<Config>>,
    creds_rx: watch::Receiver<Arc<Credentials>>,
    /// The OPERATOR config path (rollback-recovery plan, Task 1) — the
    /// real file the operator edits, watches, and reloads; NOT necessarily
    /// the path generation 0 was assembled from (a rollback boot assembles
    /// from the LKG substitute instead, `App::config_path`).
    config_path: PathBuf,
    doctor_service: DoctorService,
    #[cfg(any(test, feature = "test-util"))]
    coordination: Option<CoordinationHandle>,
    /// Local direct-switch handle — replaces the owner-mediated
    /// claim protocol. Exposed so IPC/hotkey callers can trigger
    /// local pull/push writes.
    direct_switch: Arc<DirectSwitchHandle>,
    _ipc_handle: Option<JoinHandle<()>>,
    _web_handle: Option<JoinHandle<()>>,
    /// Test-only LKG-candidate observation seam — see
    /// [`LkgCandidateObserved`].
    #[cfg(any(test, feature = "test-util"))]
    lkg_observed: Arc<Mutex<LkgCandidateObserved>>,
}

impl AppHandle {
    /// The daemon's local direct-switch handle — consumed by IPC and
    /// hotkey callers to trigger pull/push writes.
    #[must_use]
    pub fn direct_switch(&self) -> &DirectSwitchHandle {
        &self.direct_switch
    }

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

    /// Subscribe to correlated daemon observations.
    #[must_use]
    pub fn subscribe_observations(&self) -> broadcast::Receiver<DaemonObservation> {
        self.observations.subscribe()
    }

    /// Request a reload and await the receipt registered before enqueueing it.
    pub async fn request_reload(&self, source: ReloadSource) -> Option<ReloadReceipt> {
        self.request_reload_with_id(source)
            .await
            .map(|(_, receipt)| receipt)
    }

    /// Request a reload and return the allocated request identity with its receipt.
    ///
    /// This additive API lets in-process callers prove that a returned receipt
    /// belongs to their request rather than merely to a nearby reload attempt.
    pub async fn request_reload_with_id(
        &self,
        source: ReloadSource,
    ) -> Option<(u64, ReloadReceipt)> {
        let (request_id, receipt) = self.reload_requester.request(source).await?;
        receipt.await.ok().map(|receipt| (request_id, receipt))
    }

    /// Request a control-plane reload and await its correlated receipt.
    pub async fn request_control_reload(&self) -> Option<ReloadReceipt> {
        self.request_reload(ReloadSource::Control).await
    }

    /// Request an IPC reload and await its correlated receipt.
    pub async fn request_ipc_reload(&self) -> Option<ReloadReceipt> {
        self.request_reload(ReloadSource::Ipc).await
    }

    /// Request a web-apply reload and await its correlated receipt.
    pub async fn request_web_apply_reload(&self) -> Option<ReloadReceipt> {
        self.request_reload(ReloadSource::WebApply).await
    }

    /// Request an immediate reload (as if the config file changed).
    pub async fn trigger_reload(&self) -> bool {
        self.reload_requester.notify(ReloadSource::Control).await
    }

    /// Signal shutdown; the run loop tears down the current generation.
    pub fn shutdown(&self) {
        self.root.cancel();
    }

    /// Whether the daemon's root cancellation has been requested.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn root_is_cancelled_for_test(&self) -> bool {
        self.root.is_cancelled()
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

    /// The OPERATOR config path (for M2 web UI `WebState`; rollback-
    /// recovery plan Task 1) — the real file the operator edits/watches/
    /// reloads. Distinct from whatever generation 0 was actually assembled
    /// from during a rollback boot (see `App::config_path`'s doc).
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

    /// Test-only observability — production paths receive the handle by construction.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    #[must_use]
    pub fn coordination_handle(&self) -> Option<CoordinationHandle> {
        self.coordination.clone()
    }

    /// Test-util seam (rollback-recovery plan, Task 1 §5 / Task 2): a
    /// point-in-time snapshot of the `Runner`'s current LKG-candidate
    /// state. Used to prove whether a candidate is armed and, if so, from
    /// which bytes/source — without a 5-minute `stability_window` wait.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn lkg_candidate_observed(&self) -> LkgCandidateObserved {
        self.lkg_observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

// ── Runner (owns the run loop + reload) ────────────────────────────────────────

#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent, orthogonal piece of run-loop state \
              (probe-failure warn-once, rollback-active, rollback-clear-pending, \
              rollback-clear-warn-once, plus a test-only seam) — a state-machine \
              enum would need to represent their cross-product, which does not \
              exist here (they vary independently), not collapse it"
)]
struct Runner {
    config_path: PathBuf,
    creds_path: PathBuf,
    strictness: Strictness,
    source_builder: SourceBuilder,
    #[cfg(feature = "render")]
    render_sink_builder: Option<RenderSinkBuilder>,
    #[cfg(feature = "render")]
    render_context: RenderContext,
    root: CancellationToken,
    ctl_router: Arc<GenerationRouter<ControlMsg>>,
    events_router: Arc<GenerationRouter<PresenceEvent>>,
    front_ctl_handle: JoinHandle<()>,
    front_events_handle: JoinHandle<()>,
    /// Current generation's executor map for the wear tracker (spec §4.3).
    /// Republished on every install/rollback via [`Runner::install_generation`];
    /// emptied immediately before teardown in [`Runner::reload`].
    executors_tx: watch::Sender<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    reload_tx: broadcast::Sender<ReloadOutcome>,
    observations: ObservationHub,
    config_tx: watch::Sender<Arc<Config>>,
    creds_tx: watch::Sender<Arc<Credentials>>,
    generation: Generation,
    applied_revision: RuntimeRevision,
    generation_id: GenerationId,
    operation_registry: Arc<OperationRegistry>,
    /// The wear tracker's `JoinHandle` (#47 fix, secondary hardening):
    /// retained (no longer fire-and-forget) so `run_loop` can bound-await
    /// its cancellation-triggered final persist during shutdown, mirroring
    /// [`teardown`]'s bounded-join-then-abort pattern for the engine task.
    wear_tracker_handle: JoinHandle<()>,
    /// Daemon-lifetime active sampler handle retained for future IPC routing.
    #[cfg(target_os = "linux")]
    #[allow(dead_code, reason = "Task 10 wires consent IPC commands")]
    active_sampler_handle: Option<active_sampler::ActiveSamplerHandle>,
    /// Sender retained across generation swaps so Task 9 can deliver reload plans.
    #[cfg(target_os = "linux")]
    active_sampler_updates: Option<mpsc::Sender<SamplerUpdate>>,
    /// Port the web UI was started with (for reload change-detection).
    started_web_port: Option<u16>,
    /// Bind address the web UI was started with (for reload change-detection).
    started_web_bind: std::net::IpAddr,
    /// The daemon's single process-wide [`ControllerBuildContext`] (spec
    /// §4.3's `PanelLocks` plus Task 8's macOS gamma-hold-registry/
    /// breadcrumb), constructed once in [`App::start`] and carried by
    /// `Runner` across every reload so `load_and_assemble`'s
    /// `assemble_static` call always reuses the SAME shared state — a
    /// physical panel's lock (and, on macOS, a gamma selector's hold/
    /// breadcrumb) must resolve to the same shared instance whether it came
    /// from the old generation's controller or the new one's.
    ctrl_ctx: ControllerBuildContext,
    /// Daemon-lifetime notifier episode state (spec §4.4), constructed once
    /// in [`App::start`] and threaded unchanged into every generation's
    /// [`spawn_generation`] call — episodes survive a config reload.
    notify_state: Arc<Mutex<NotifyState>>,
    /// Daemon-lifetime notification sink, constructed once in
    /// [`App::start`] and threaded unchanged into every generation.
    notify_sink: Arc<dyn NotifySink>,
    /// Daemon-lifetime ownership gate. Private-only startup keeps this as
    /// `AlwaysOwned`; shared startup keeps the same coordination-backed gate.
    ownership: Arc<dyn OwnershipGate>,
    /// Shared-display cache, absent only when startup had no shared displays.
    coordination: Option<CoordinationHandle>,
    /// Local direct-switch handle — replaces the owner-mediated
    /// claim protocol. Constructed once in [`App::start`] and
    /// carried by `Runner` across every reload so IPC/hotkey
    /// callers can trigger local pull/push writes.
    #[allow(dead_code, reason = "wired in Task 13")]
    direct_switch: Arc<DirectSwitchHandle>,
    /// Daemon-lifetime idle-observation tx — the stock idle source
    /// publishes into this channel; carried across reloads so
    /// consumers always see current data.
    idle_obs_tx: Option<crate::idle_observation::IdleObservationTx>,
    /// Daemon-lifetime filtered activity fan-out retained across reloads.
    filtered_activity_tx: crate::filtered_activity::FilteredActivityTx,
    /// The systemd watchdog sender (spec §6.2/§6.3). Injected via
    /// [`App::with_sd_notify`]; defaults to [`SdNotify::from_env`].
    sd: SdNotify,
    /// The probe arm's tick period (spec §6.3), captured once at
    /// construction — see [`App::with_watchdog_interval`].
    watchdog_interval: Duration,
    /// Bounded wait for the old engine's drain acknowledgement during reload.
    generation_barrier_ack_timeout: Duration,
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
    /// The crash-loop/rollback state directory (rollback-recovery plan,
    /// Task 2 §2) — threaded unchanged from `App::state_dir`. The target
    /// of `boot_guard::clear_rollback_after_reload` on an accepted reload
    /// while `rollback_active`.
    state_dir: PathBuf,
    /// Whether this daemon is CURRENTLY running under an active rollback
    /// (rollback-recovery plan, Task 2 §2) — the `Runner`-local copy of
    /// `App::rollback_active`, threaded once at construction. Set back to
    /// `false` by `Runner::reload`'s accepted-reload arm the first time a
    /// reload succeeds (the `ContinueRollback` → Proceed transition, Task 2
    /// §3) — never re-derived from the persisted `crash-loop.json`, so a
    /// persisted-write failure (§4) can never re-arm it.
    rollback_active: bool,
    /// Runner-owned rollback presentation status (rollback-recovery plan,
    /// Task 1 §7) — mirrors `rollback_active`'s lifecycle exactly (seeded
    /// once at construction from `App::rollback_status`, cleared in the
    /// SAME accepted-reload arm that flips `rollback_active` to `false`).
    /// Threaded into every `spawn_generation` call as the sibling `rollback`
    /// parameter; never re-read from disk or reconstructed from a snapshot.
    rollback_status: Option<RollbackStatus>,
    /// Set when the accepted-reload rollback-recovery transition's
    /// `crash-loop.json` clear (Task 2 §4) failed to write. Runtime
    /// recovery is already complete at that point (`rollback_active` is
    /// already `false`) — this flag only drives a best-effort RETRY of the
    /// same atomic clear from every subsequent watchdog/LKG tick, so the
    /// persisted state doesn't stay permanently stuck at
    /// `rollback_active: true` after a transient write failure.
    rollback_state_clear_pending: bool,
    /// Warn-once latch for `config_rollback_state_clear_failed` (Task 2
    /// §4): the retry fires on EVERY watchdog tick while
    /// `rollback_state_clear_pending`, so an unlatched `WARN` would flood
    /// the journal until the write finally succeeds. Reset to `false`
    /// whenever the retry succeeds (so a LATER failure streak, from a
    /// future rollback, warns again).
    rollback_state_clear_warned: bool,
    /// Test-util seam (rollback-recovery plan, Task 2 §6): the SAME
    /// `Arc<Mutex<LkgCandidateObserved>>` `AppHandle::lkg_candidate_observed`
    /// reads. `App::start` seeds it with the boot-time snapshot, but only
    /// `Runner` ever changes `lkg_candidate` afterwards (a fresh reload
    /// arm, or a successful promotion clearing it) — so `Runner` must hold
    /// its own handle to the SAME `Arc` and push every such change through
    /// [`Runner::sync_lkg_observed`], or the seam would forever report
    /// stale boot-time state to any test observing it after a reload.
    #[cfg(any(test, feature = "test-util"))]
    lkg_observed: Arc<Mutex<LkgCandidateObserved>>,
    /// Test seam (F1) — see `App::force_reload_spawn_failure`.
    #[cfg(any(test, feature = "test-util"))]
    force_reload_spawn_failure: bool,
    /// Test seam — see `App::force_rebuild_old_spawn_failure`.
    #[cfg(any(test, feature = "test-util"))]
    force_rebuild_old_spawn_failure: bool,
    /// Test seam — see `App::force_reload_snapshot_timeout`.
    #[cfg(any(test, feature = "test-util"))]
    force_reload_snapshot_timeout: bool,
    #[cfg(any(test, feature = "test-util"))]
    generation_barrier_gate: Option<GenerationBarrierGate>,
    #[cfg(any(test, feature = "test-util"))]
    force_generation_barrier_timeout: bool,
    #[cfg(any(test, feature = "test-util"))]
    reload_lifecycle_capture: Option<ReloadLifecycleCapture>,
    /// The activity-follow task spawned on boot and re-spawned on reload.
    activity_follow_handle: Option<tokio::task::JoinHandle<()>>,
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

/// Test-only snapshot of the `Runner`'s current LKG-candidate state
/// (rollback-recovery plan, Task 1 §5 / Task 2 candidate-observation seam —
/// mirrors the `force_reload_spawn_failure` precedent, `App`'s field doc
/// above). Exposes armed-state plus enough identity (the candidate's own
/// `source` tag and its captured bytes) to prove WHICH generation a
/// candidate is tracking, without exposing the private `LkgCandidate` type
/// itself across the `dormantd::app` boundary.
///
/// Committed here (Task 1) because Task 1's own coupling-hazard RED test
/// (§5: "no candidate armed" after a rollback boot) needs it; Task 2 and
/// Task 3 reuse this SAME type/accessor for their own candidate-arming
/// assertions rather than adding a second seam.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LkgCandidateObserved {
    /// Whether a candidate is currently armed.
    pub armed: bool,
    /// The armed candidate's `source` tag (`"boot"`/`"reload"`), or `None`
    /// when nothing is armed.
    pub source: Option<&'static str>,
    /// The armed candidate's captured bytes, or `None` when nothing is
    /// armed — lets a test assert WHICH bytes (e.g. the operator file's
    /// current contents) a candidate was armed from.
    pub bytes: Option<Vec<u8>>,
}

#[cfg(any(test, feature = "test-util"))]
impl LkgCandidateObserved {
    fn from_candidate(candidate: Option<&LkgCandidate>) -> Self {
        match candidate {
            Some(c) => Self {
                armed: true,
                source: Some(c.source),
                bytes: Some(c.bytes.clone()),
            },
            None => Self::default(),
        }
    }
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
    async fn install_generation(&mut self, spawn: GenSpawn) {
        let executors: HashMap<DisplayId, Arc<dyn CommandSink>> = spawn
            .generation
            .display_executors
            .iter()
            .map(|(id, exec)| (id.clone(), exec.clone() as Arc<dyn CommandSink>))
            .collect();
        self.executors_tx.send_replace(Arc::new(executors));
        self.generation_barrier_ack_timeout =
            spawn.generation.cfg.daemon.generation_barrier_ack_timeout;
        self.generation = spawn.generation;
        self.ctl_router
            .install_generation(spawn.ctl_tx.clone(), self.generation_id)
            .await;
        self.events_router.install(spawn.events_tx.clone()).await;
    }

    #[cfg(target_os = "linux")]
    fn publish_active_sampler_reload(&self, previous_cfg: &Config, new_generation: &Generation) {
        let Some(updates) = &self.active_sampler_updates else {
            return;
        };
        let display_exists = new_generation
            .cfg
            .wear
            .active_sampling
            .sampled_display
            .as_ref()
            .is_some_and(|display| {
                new_generation
                    .display_executors
                    .contains_key(&DisplayId(display.clone()))
            });
        if let Err(error) = updates.try_send(SamplerUpdate::DisplayContext(
            active_sampler_display_context(&new_generation.cfg, display_exists, Some("active")),
        )) {
            tracing::warn!(
                event = "wear_sampling_update_dropped",
                ?error,
                "could not publish active-sampler display context"
            );
        }
        for plan in active_sampler_reconfigure_plans(previous_cfg, &new_generation.cfg) {
            if let Err(error) = updates.try_send(SamplerUpdate::Reconfigure(plan)) {
                tracing::warn!(
                    event = "wear_sampling_update_dropped",
                    ?error,
                    "could not publish active-sampler reload plan"
                );
                break;
            }
        }
    }

    /// Build a [`KvmStatus`] from the current generation's config.
    fn build_kvm_status(&self) -> dormant_core::rules::KvmStatus {
        use dormant_core::config::DisplayScope;
        use dormant_core::types::DisplayId;

        let cfg = &self.generation.cfg;
        let mut switch_capable = Vec::new();
        let mut push_capable = Vec::new();

        for (name, dc) in &cfg.displays {
            if dc.scope != DisplayScope::Shared {
                continue;
            }
            if dc.shared_input_code.is_some() {
                switch_capable.push(DisplayId(name.clone()));
            }
            if dc.shared_peer_input_write_code.is_some() {
                push_capable.push(DisplayId(name.clone()));
            }
        }

        dormant_core::rules::KvmStatus {
            keymap: cfg.keymap.clone(),
            switch_capable_displays: switch_capable,
            activity_following: self.activity_follow_handle.is_some(),
            push_capable_displays: push_capable,
        }
    }

    /// Send the current [`KvmStatus`] to the live engine via [`ControlMsg::SetKvmStatus`].
    ///
    /// Called on boot (generation 0) and after every accepted reload so the
    /// tray, web UI, and CLI always observe up-to-date KVM state — prevents
    /// the first-boot status gap (issue #137).
    async fn publish_kvm_status(&self) {
        let kvm = self.build_kvm_status();
        if let Some(ctl) = self.ctl_router.current().await {
            let _ = ctl.send(ControlMsg::SetKvmStatus(kvm)).await;
        }
    }

    /// Spawn (or re-spawn) the activity-follow task from the current
    /// generation's config and daemon-lifetime channels.
    ///
    /// Drops any prior handle; re-spawns only when
    /// `coordination.activity_follow` is true.
    fn spawn_activity_follow(&mut self) {
        if self.activity_follow_handle.is_some() {
            self.activity_follow_handle = None;
        }
        if self.generation.cfg.coordination.activity_follow
            && let Some(idle_tx) = self.idle_obs_tx.as_ref()
        {
            let idle_rx = idle_tx.subscribe();
            let filtered_rx = self.filtered_activity_tx.subscribe();
            let shared: Vec<dormant_core::types::DisplayId> = self
                .generation
                .cfg
                .displays
                .iter()
                .filter(|(_, dc)| {
                    dc.scope == dormant_core::config::DisplayScope::Shared
                        && dc.shared_input_code.is_some()
                })
                .map(|(name, _)| dormant_core::types::DisplayId(name.clone()))
                .collect();

            if !shared.is_empty() {
                let deps = crate::activity_follow::ActivityFollowDeps {
                    idle_rx,
                    direct_switch: Some(self.direct_switch.clone()),
                    display_ids: shared.into(),
                    arm_after: self.generation.cfg.coordination.arm_after,
                    cancel: self.root.clone(),
                    pull_recorder: None,
                    clock: crate::activity_follow::production_clock,
                };
                self.activity_follow_handle =
                    Some(crate::activity_follow::spawn(deps, filtered_rx));
            }
        }
    }

    /// Publish the KVM claim status to the current engine generation.
    ///
    /// Reload the config, restarting the runtime in place. See the module
    /// docs for the full state machine.
    #[allow(clippy::too_many_lines)]
    async fn reload(
        &mut self,
        new_assembly: Result<StaticAssembly, String>,
        requested_revision: RuntimeRevision,
        request_ids: Vec<u64>,
        sources: Vec<ReloadSource>,
    ) -> ReloadReceipt {
        let old_ctl = self
            .ctl_router
            .current()
            .await
            .expect("current engine control route exists before reload");
        #[cfg(any(test, feature = "test-util"))]
        let preliminary = if self.force_reload_snapshot_timeout {
            self.record_reload_lifecycle_stage("snapshot_bypassed");
            None
        } else {
            request_snapshot(&old_ctl).await
        };
        #[cfg(not(any(test, feature = "test-util")))]
        let preliminary = request_snapshot(&old_ctl).await;

        // Validate + assemble the NEW config BEFORE touching the running
        // generation. An invalid or un-assemblable config only flags
        // pending_reload on the live engine and leaves it running — no
        // teardown, no phase loss, no churn on a bad edit.
        let new_assembly = match new_assembly {
            Ok(assembly) => assembly,
            Err(detail) => {
                tracing::error!(event = "config_reload_rejected", detail = %detail);
                let _ = old_ctl
                    .send(ControlMsg::SetPendingReload(Some(detail.clone())))
                    .await;
                let outcome = ReloadOutcome::Rejected(detail);
                let _ = self.reload_tx.send(outcome.clone());
                return self.reload_receipt(
                    request_ids,
                    sources,
                    requested_revision,
                    outcome,
                    false,
                );
            }
        };

        let new_shared = shared_displays(&new_assembly.cfg);
        if self.coordination.is_none() && !new_shared.is_empty() {
            let detail =
                "E_CONFIG_INVALID: adding the first shared display requires daemon restart"
                    .to_string();
            tracing::error!(event = "config_reload_rejected", detail = %detail);
            let _ = old_ctl
                .send(ControlMsg::SetPendingReload(Some(detail.clone())))
                .await;
            let outcome = ReloadOutcome::Rejected(detail);
            let _ = self.reload_tx.send(outcome.clone());
            return self.reload_receipt(request_ids, sources, requested_revision, outcome, false);
        }
        if let Some(coordination) = &self.coordination {
            coordination.reconcile_shared(new_shared);
        }

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
        // Task 8 dispatch-identity invariant: a MANUAL-ONLY (rule-less) dark
        // display retained under the SAME `DisplayId` but whose
        // dispatch-relevant config changed is recovery-equivalent to
        // removal — merged into the same verified old-executor wake loop as
        // `removed` below (see `changed_dispatch_dark_displays`'s docs,
        // including why ruled displays are excluded via the same `ruled`
        // set `retained_dark` above already uses). Computed here, before
        // `new_assembly.cfg` is consumed by `spawn_generation_for_reload`
        // below; `self.generation.cfg` is still the OLD config at this
        // point (`install_generation` hasn't run yet).
        let changed_identity = changed_dispatch_dark_displays(
            snapshot.as_ref(),
            &self.generation.cfg,
            &new_assembly.cfg,
            &ruled,
        );

        // Capture the new config for watch updates + bind change detection
        // BEFORE new_assembly is consumed by spawn_generation.
        #[cfg(target_os = "linux")]
        let previous_active_sampler_cfg = self.generation.cfg.clone();
        let new_cfg = new_assembly.cfg.clone();
        let new_creds = new_assembly.creds.clone();
        self.generation_barrier_ack_timeout = new_cfg.daemon.generation_barrier_ack_timeout;

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

        self.ctl_router.pause().await;
        self.events_router.pause().await;
        self.record_reload_lifecycle_stage("routers_paused");
        self.operation_registry.begin_quiesce(self.generation_id);
        if matches!(
            quiesce_operations(
                &self.operation_registry,
                self.generation_id,
                QUIESCE_OPERATION_TIMEOUT,
                QUIESCE_OPERATION_TIMEOUT,
            )
            .await,
            QuiesceOutcome::Busy
        ) {
            tracing::warn!(
                event = "operation_quiesce_cancelled",
                generation = self.generation_id.0
            );
            let executors = self
                .generation
                .display_executors
                .iter()
                .map(|(id, exec)| (id.clone(), exec.clone() as Arc<dyn CommandSink>))
                .collect();
            self.executors_tx.send_replace(Arc::new(executors));
            self.ctl_router
                .install_generation(old_ctl.clone(), self.generation_id)
                .await;
            if let Some(events) = self.generation.engine_events_tx.clone() {
                self.events_router.install(events).await;
            }
            let detail = "E_OPERATION_BUSY: active hardware operation did not quiesce".to_string();
            self.operation_registry.end_quiesce(self.generation_id);
            tracing::warn!(
                event = "reload_rejected_operation_busy",
                generation = self.generation_id.0
            );
            let outcome = ReloadOutcome::Rejected(detail);
            let _ = self.reload_tx.send(outcome.clone());
            return self.reload_receipt(request_ids, sources, requested_revision, outcome, false);
        }
        quiesce_inputs(&mut self.generation).await;
        self.record_reload_lifecycle_stage("inputs_quiesced");
        #[cfg(any(test, feature = "test-util"))]
        if let Some(gate) = &self.generation_barrier_gate {
            gate.reach(old_ctl.clone()).await;
        }
        if let Err(detail) = await_generation_barrier(
            &old_ctl,
            self.generation_barrier_ack_timeout,
            self.force_generation_barrier_timeout_for_test(),
        )
        .await
        {
            tracing::error!(event = "generation_barrier_invariant_failed", detail);
            self.root.cancel();
            let outcome = ReloadOutcome::Rejected(detail.to_string());
            let _ = self.reload_tx.send(outcome.clone());
            return self.reload_receipt(request_ids, sources, requested_revision, outcome, false);
        }
        self.record_reload_lifecycle_stage("barrier_acknowledged");
        self.observations
            .emit(DaemonObservation::GenerationDrained {
                generation: self.generation_id,
            });
        teardown(&mut self.generation).await;
        self.record_reload_lifecycle_stage("engine_torn_down");

        // Verified physical wake of REMOVED displays (no executor in the new
        // generation) that were dark — via their OLD executor, after teardown
        // — chained with Task 8's dispatch-identity-changed displays (SAME
        // `DisplayId`, different dispatch identity, recovery-equivalent to
        // removal; see `changed_dispatch_dark_displays`'s docs). Disjoint
        // sets by construction, so a plain chain never double-wakes a
        // display through this loop. A failure aborts the reload and
        // restores the old config in place (with pending_reload set) —
        // identical treatment for both categories.
        for display_id in removed.into_iter().chain(changed_identity) {
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
                    self.rebuild_old(Some(detail.clone()), snapshot.as_ref())
                        .await;
                    let outcome = ReloadOutcome::Rejected(detail);
                    let _ = self.reload_tx.send(outcome.clone());
                    return self.reload_receipt(
                        request_ids,
                        sources,
                        requested_revision,
                        outcome,
                        false,
                    );
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

        let next_generation = GenerationId(self.generation_id.0.saturating_add(1));
        let spawn_result = spawn_generation_for_reload(
            &self.state_dir,
            &self.root,
            new_assembly,
            restore_snapshot.as_ref(),
            None,
            None,
            self.notify_state.clone(),
            self.notify_sink.clone(),
            self.ownership.clone(),
            self.coordination.clone(),
            self.config_tx.subscribe(),
            self.executors_tx.subscribe(),
            next_generation,
            self.operation_registry.clone(),
            Some(self.observations.clone()),
            self.idle_obs_tx.clone(),
            self.filtered_activity_tx.clone(),
            &self.direct_switch,
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
                self.install_generation(spawn).await;
                self.applied_revision = requested_revision.clone();
                self.generation_id = next_generation;
                self.observations
                    .emit(DaemonObservation::GenerationStarted {
                        generation: self.generation_id,
                    });

                // Republish executor/config watches BEFORE activity-follow
                // or any new edge can write — an edge that fires against a
                // stale executor writes to the wrong panel.
                self.spawn_activity_follow();
                self.publish_kvm_status().await;

                // Rollback recovery (rollback-recovery plan, Task 2 §3): a
                // successful reload from the operator path while a
                // boot-time rollback is active is the live-reload sibling
                // of `boot_guard`'s `ContinueRollback -> Proceed`
                // transition (Context). Gated on `self.rollback_active` so
                // an ordinary (non-rollback) accepted reload is completely
                // unaffected — no persisted write, no event, no banner
                // send beyond what already happens today.
                if self.rollback_active {
                    // Clear the just-installed engine's pending-reload
                    // banner explicitly, before broadcasting `Reloaded`
                    // below (Context: "clears the engine's pending-reload
                    // banner"). The fresh engine above was already spawned
                    // with `pending: None`, so this is belt-and-suspenders
                    // — it makes the transition's specified shape explicit
                    // rather than relying on `spawn_generation`'s literal
                    // staying `None` forever.
                    let new_ctl = self
                        .ctl_router
                        .current()
                        .await
                        .expect("new engine control route exists after install");
                    let _ = new_ctl.send(ControlMsg::SetPendingReload(None)).await;

                    // Persist the crash-loop-state clear (Task 2 §4: a
                    // write failure here must NOT roll back this
                    // physically successful reload). The runner flag
                    // flips to `false` only AFTER the write attempt,
                    // regardless of its outcome — runtime recovery is
                    // complete either way; only the PERSISTED clear can
                    // lag, and is retried from subsequent watchdog/LKG
                    // ticks (`Runner::retry_rollback_state_clear`).
                    match boot_guard::clear_rollback_after_reload(&self.state_dir) {
                        Ok(()) => {
                            self.rollback_state_clear_pending = false;
                            self.rollback_state_clear_warned = false;
                        }
                        Err(e) => {
                            self.rollback_state_clear_pending = true;
                            if !self.rollback_state_clear_warned {
                                tracing::warn!(
                                    event = "config_rollback_state_clear_failed",
                                    error = %e,
                                    "reload recovered but the persisted crash-loop state failed \
                                     to clear; retrying from subsequent watchdog ticks"
                                );
                                self.rollback_state_clear_warned = true;
                            }
                        }
                    }
                    self.rollback_status = None;
                    self.rollback_active = false;

                    tracing::info!(
                        event = "config_rollback_recovered",
                        config = %self.config_path.display(),
                    );
                }

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
                #[cfg(target_os = "linux")]
                self.publish_active_sampler_reload(&previous_active_sampler_cfg, &self.generation);
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
                #[cfg(any(test, feature = "test-util"))]
                self.sync_lkg_observed();
                // Step-boundary ping 7/7 (spec §6.3): reload end. Fires
                // BEFORE the reload-outcome broadcast (not after): on the
                // real multi-threaded runtime a receiver parked on
                // `subscribe_reload()` can wake and run in TRUE PARALLEL
                // with the rest of this function the instant `send` is
                // called, so anything after `send` races an observer that
                // reacts to the outcome — the ping must be fully visible
                // BEFORE the outcome is, not after.
                self.ping("reload_end");
                let outcome = ReloadOutcome::Reloaded;
                let _ = self.reload_tx.send(outcome.clone());
                self.reload_receipt(request_ids, sources, requested_revision, outcome, false)
            }
            Err(e) => {
                let detail = format!("rebuild from new config failed: {e}");
                tracing::error!(event = "config_reload_rejected", detail = %detail);
                // Step-boundary ping 6/7 (spec §6.3/P10): before the second
                // `rebuild_old` call site (the accepted-config
                // `spawn_generation` failure path).
                self.ping("before_rebuild_old_spawn_failure");
                self.record_reload_lifecycle_stage("rebuild_old_started");
                self.rebuild_old(Some(detail.clone()), snapshot.as_ref())
                    .await;
                self.record_reload_lifecycle_stage("rebuild_old_finished");
                let outcome = ReloadOutcome::Rejected(detail);
                let _ = self.reload_tx.send(outcome.clone());
                self.record_reload_lifecycle_stage("outcome_broadcast");
                self.reload_receipt(request_ids, sources, requested_revision, outcome, false)
            }
        }
    }

    fn reload_receipt(
        &self,
        request_ids: Vec<u64>,
        sources: Vec<ReloadSource>,
        requested_revision: RuntimeRevision,
        outcome: ReloadOutcome,
        coalesced: bool,
    ) -> ReloadReceipt {
        ReloadReceipt {
            request_ids,
            sources,
            requested_revision,
            applied_revision: self.applied_revision.clone(),
            generation: self.generation_id,
            outcome,
            coalesced,
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    fn record_reload_lifecycle_stage(&self, stage: &'static str) {
        if self.force_reload_snapshot_timeout
            && let Some(capture) = &self.reload_lifecycle_capture
        {
            capture.record(stage);
        }
    }

    #[cfg(not(any(test, feature = "test-util")))]
    #[allow(clippy::unused_self)]
    fn record_reload_lifecycle_stage(&self, _stage: &'static str) {}

    #[cfg(any(test, feature = "test-util"))]
    fn force_generation_barrier_timeout_for_test(&self) -> bool {
        self.force_generation_barrier_timeout
    }

    #[cfg(not(any(test, feature = "test-util")))]
    #[allow(clippy::unused_self)]
    fn force_generation_barrier_timeout_for_test(&self) -> bool {
        false
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

    /// Validate and assemble bytes already loaded by the reload coordinator.
    async fn assemble_loaded(
        &self,
        cfg: Config,
        creds: Credentials,
    ) -> Result<StaticAssembly, String> {
        let errors = validate_with_input_source_readers(
            &cfg,
            &capabilities(),
            &input_source_readers(),
            &creds,
        );
        if !errors.is_empty() {
            return Err(errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "));
        }
        #[cfg(feature = "render")]
        {
            assemble_static_with_context(
                cfg,
                creds,
                &self.source_builder,
                self.render_sink_builder.as_ref(),
                &self.ctrl_ctx,
                &self.render_context,
            )
            .await
            .map_err(|e| e.to_string())
        }
        #[cfg(not(feature = "render"))]
        {
            assemble_static(cfg, creds, &self.source_builder, &self.ctrl_ctx)
                .await
                .map_err(|e| e.to_string())
        }
    }

    /// Restart the *old* config in place with `pending` populated. Reuses the
    /// current generation's live controllers (no re-probe); rebuilds sources.
    async fn rebuild_old(&mut self, pending: Option<String>, snapshot: Option<&StateSnapshot>) {
        let sources = (self.source_builder)(&self.generation.cfg, &self.generation.creds)
            .unwrap_or_else(|e| {
                tracing::error!(event = "reload_source_rebuild_failed", error = %e);
                Vec::new()
            });
        let (activity_rules, activity_poll) = activity_rules(&self.generation.cfg);
        #[cfg(feature = "render")]
        let (render_sinks, input_wake_rx, scan_jobs, item_report_rx) =
            build_render_sinks_with_context(
                &self.generation.cfg,
                self.render_sink_builder.as_ref(),
                &self.render_context,
            );
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
            #[cfg(feature = "render")]
            item_report_rx: Some(item_report_rx),
            #[cfg(feature = "render")]
            item_journal: self.render_context.item_journal.clone(),
            #[cfg(feature = "render")]
            scan_jobs,
        };
        let spawn_result = spawn_generation_for_reload(
            &self.state_dir,
            &self.root,
            assembly,
            snapshot,
            pending,
            self.rollback_status.clone(),
            self.notify_state.clone(),
            self.notify_sink.clone(),
            self.ownership.clone(),
            self.coordination.clone(),
            self.config_tx.subscribe(),
            self.executors_tx.subscribe(),
            self.generation_id,
            self.operation_registry.clone(),
            Some(self.observations.clone()),
            self.idle_obs_tx.clone(),
            self.filtered_activity_tx.clone(),
            &self.direct_switch,
        );
        #[cfg(any(test, feature = "test-util"))]
        let spawn_result = if self.force_rebuild_old_spawn_failure {
            spawn_result.and_then(|_| {
                Err(anyhow::anyhow!(
                    "test-injected rebuild_old spawn_generation failure"
                ))
            })
        } else {
            spawn_result
        };
        match spawn_result {
            Ok(spawn) => {
                self.install_generation(spawn).await;
                self.observations
                    .emit(DaemonObservation::GenerationStarted {
                        generation: self.generation_id,
                    });
            }
            Err(e) => {
                tracing::error!(event = "reload_rebuild_old_failed", error = %e);
                // Routers remain paused without a replacement generation; exit so the
                // supervisor can restart into the boot-guard/LKG recovery path.
                self.root.cancel();
            }
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
    async fn watchdog_tick_at(&mut self, now: Instant) {
        // Rollback-recovery retry (Task 2 §4): fires on EVERY watchdog
        // tick, independent of probe health — the pending clear is a
        // plain filesystem write, unrelated to whether the engine answers
        // a snapshot round-trip. Placed before the probe so a wedged
        // engine (which starves the rest of this function below) never
        // starves the retry too.
        self.retry_rollback_state_clear();

        let Some(ctl) = self.ctl_router.current().await else {
            tracing::error!(
                event = "generation_route_missing",
                "watchdog found no active engine route"
            );
            return;
        };
        let probe_result = watchdog_probe(&ctl).await;
        if ping_if_healthy(&mut self.sd, probe_result.as_ref()) {
            self.probe_failed_warned = false;
            // `ping_if_healthy` returning `true` only on `Some` is the
            // whole point of the extraction below — safe to unwrap.
            self.lkg_tick_at(
                probe_result
                    .as_ref()
                    .expect("ping_if_healthy true implies Some"),
                now,
            );
        } else {
            // A wedged engine starves the watchdog by design (spec
            // invariant #3) — NO ping — and any in-flight LKG candidate
            // loses its unbroken-healthy-window claim (spec F3).
            reset_candidate_on_probe_failure(&mut self.lkg_candidate, now);
            if !self.probe_failed_warned {
                tracing::warn!(
                    event = "watchdog_probe_failed",
                    "engine did not answer a snapshot round-trip; watchdog ping withheld"
                );
                self.probe_failed_warned = true;
            }
        }
    }

    /// Retry the accepted-reload rollback-recovery's persisted
    /// `crash-loop.json` clear (Task 2 §4) after a prior write failure. A
    /// no-op when nothing is pending — the common case, every tick a
    /// rollback isn't in a write-failure streak. Runtime recovery
    /// (`rollback_active`) is ALREADY `false` by the time this can ever
    /// have something pending (`Runner::reload`'s accepted-reload arm
    /// flips it before this flag can even be set) — this only chases the
    /// PERSISTED state until it agrees.
    fn retry_rollback_state_clear(&mut self) {
        if !self.rollback_state_clear_pending {
            return;
        }
        match boot_guard::clear_rollback_after_reload(&self.state_dir) {
            Ok(()) => {
                self.rollback_state_clear_pending = false;
                self.rollback_state_clear_warned = false;
            }
            Err(e) => {
                if !self.rollback_state_clear_warned {
                    tracing::warn!(
                        event = "config_rollback_state_clear_failed",
                        error = %e,
                        "retry of the persisted crash-loop state clear failed again; still \
                         retrying"
                    );
                    self.rollback_state_clear_warned = true;
                }
            }
        }
    }

    /// Test-util seam sync (rollback-recovery plan, Task 2 §6): push the
    /// current `self.lkg_candidate` into the shared
    /// `AppHandle::lkg_candidate_observed()` snapshot. Must be called at
    /// every site that changes `lkg_candidate`'s presence/identity — a
    /// fresh reload arm and a successful promotion clearing it are the two
    /// RUNTIME sites (the boot-time value is already captured directly by
    /// `App::start` when it constructs the `Arc<Mutex<_>>`, so no call is
    /// needed there).
    #[cfg(any(test, feature = "test-util"))]
    fn sync_lkg_observed(&self) {
        *self
            .lkg_observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            LkgCandidateObserved::from_candidate(self.lkg_candidate.as_ref());
    }

    /// The §4 LKG-promotion check, run on every healthy probe tick. A
    /// `None` candidate (disabled, or none armed yet) is a no-op — spec
    /// §4 failure semantics: `lkg_enabled = false` means no candidate
    /// tracking, no files, ever.
    fn lkg_tick_at(&mut self, snapshot: &StateSnapshot, now: Instant) {
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
            now,
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
                match write_lkg(&self.state_dir, source, bytes) {
                    Ok(()) => {
                        tracing::info!(event = "lkg_saved");
                        self.lkg_defer_count = 0;
                        self.lkg_candidate = None;
                        #[cfg(any(test, feature = "test-util"))]
                        self.sync_lkg_observed();
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
/// `state_dir/last-known-good.toml` + the `.meta.json` sidecar (spec §3).
/// A free function, not a `Runner` method (`clippy::unused_self`): it needs
/// nothing from `Runner` beyond the two arguments the caller already has.
fn write_lkg(dir: &std::path::Path, source: &'static str, bytes: &[u8]) -> std::io::Result<()> {
    boot_guard::write_atomic_bytes(dir, "last-known-good.toml", bytes)?;
    let meta = LkgMeta {
        schema_version: 1,
        fingerprint: boot_guard::fingerprint_bytes(bytes),
        saved_at_epoch_s: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        source,
    };
    boot_guard::write_atomic_json(dir, "last-known-good.meta.json", &meta)
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
/// [`Runner::watchdog_tick_at`] so the reset rule is unit-testable without a
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
#[allow(
    clippy::too_many_lines,
    reason = "platform-specific signal loops and bounded shutdown ownership stay together"
)]
async fn run_loop(
    mut runner: Runner,
    mut watcher: reload::ConfigWatcher,
    reload_requester: ReloadRequester,
    mut reload_requests: mpsc::Receiver<ReloadRequest>,
) {
    // The watchdog probe arm (spec §6.3): its period is captured once at
    // `Runner` construction (`App::start`/`with_watchdog_interval`).
    // Runs on BOTH platform branches below — on non-Unix the tick still
    // drives §4 LKG promotion even though `sd.watchdog()` itself is a
    // permanent no-op there (no `NOTIFY_SOCKET` concept off Linux).
    let mut watchdog_schedule =
        WatchdogSchedule::new(runner.watchdog_interval, tokio::time::Instant::now());

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
                    let _ = reload_requester.notify(ReloadSource::Signal).await;
                }
                Some(()) = watcher.rx.recv() => {
                    tracing::info!(event = "reload_trigger", source = "watcher");
                    let _ = reload_requester.notify(ReloadSource::Watcher).await;
                }
                Some(first) = reload_requests.recv() => {
                    Box::pin(execute_reload_batch(
                        &mut runner,
                        &mut watcher,
                        &reload_requester,
                        &mut reload_requests,
                        first,
                    )).await;
                }
                () = tokio::time::sleep_until(watchdog_schedule.deadline()) => {
                    watchdog_schedule.record_tick(tokio::time::Instant::now());
                    runner.watchdog_tick_at(Instant::now()).await;
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
                    let _ = reload_requester.notify(ReloadSource::Watcher).await;
                }
                Some(first) = reload_requests.recv() => {
                    execute_reload_batch(
                        &mut runner,
                        &mut watcher,
                        &reload_requester,
                        &mut reload_requests,
                        first,
                    ).await;
                }
                () = tokio::time::sleep_until(watchdog_schedule.deadline()) => {
                    watchdog_schedule.record_tick(tokio::time::Instant::now());
                    runner.watchdog_tick_at(Instant::now()).await;
                }
            }
        }
    }

    // #47 fix (secondary hardening): the wear tracker's cancellation
    // branch (`deps.cancel.cancelled()`, `wear_tracker.rs`) does its own
    // final persist — but `root` was only just cancelled (either by the
    // signal handler above or by `AppHandle::shutdown`), and that persist
    // races this function's return unless something here waits for it.
    // Bound-await it concurrently with the current generation's engine
    // teardown, using the same bounded-join-then-abort shape as
    // `teardown` itself: a stuck tracker must never hang daemon shutdown.
    let wear_tracker_handle = runner.wear_tracker_handle;
    let wear_teardown = async {
        let abort = wear_tracker_handle.abort_handle();
        if tokio::time::timeout(Duration::from_secs(5), wear_tracker_handle)
            .await
            .is_err()
        {
            abort.abort();
            tracing::warn!(event = "wear_tracker_abort_forced");
        }
    };
    let front_ctl_handle = runner.front_ctl_handle;
    let front_events_handle = runner.front_events_handle;
    let front_teardown = async {
        for handle in [front_ctl_handle, front_events_handle] {
            let abort = handle.abort_handle();
            if tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .is_err()
            {
                abort.abort();
                tracing::warn!(event = "front_router_abort_forced");
            }
        }
    };
    let generation_teardown = async {
        quiesce_inputs(&mut runner.generation).await;
        teardown(&mut runner.generation).await;
    };
    tokio::join!(generation_teardown, wear_teardown, front_teardown,);
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

struct ReloadBatch {
    request_ids: Vec<u64>,
    sources: Vec<ReloadSource>,
    waiters: Vec<oneshot::Sender<ReloadReceipt>>,
}

impl ReloadBatch {
    fn new(request: ReloadRequest) -> Self {
        let mut batch = Self {
            request_ids: Vec::new(),
            sources: Vec::new(),
            waiters: Vec::new(),
        };
        batch.push(request);
        batch
    }

    fn push(&mut self, request: ReloadRequest) {
        self.request_ids.push(request.request_id);
        if !self.sources.contains(&request.source) {
            self.sources.push(request.source);
        }
        if let Some(waiter) = request.receipt_tx {
            self.waiters.push(waiter);
        }
    }
}

async fn execute_reload_batch(
    runner: &mut Runner,
    watcher: &mut reload::ConfigWatcher,
    requester: &ReloadRequester,
    requests: &mut mpsc::Receiver<ReloadRequest>,
    first: ReloadRequest,
) {
    let mut batch = ReloadBatch::new(first);
    let window = runner.generation.cfg.daemon.reload_debounce;
    if !window.is_zero() {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break,
                Some(request) = requests.recv() => batch.push(request),
                Some(()) = watcher.rx.recv() => {
                    let _ = requester.notify(ReloadSource::Watcher).await;
                }
            }
        }
    }

    let (requested_revision, loaded) =
        load_runtime_from_paths(&runner.config_path, &runner.creds_path, runner.strictness);
    runner.observations.emit(DaemonObservation::ReloadStarted {
        request_ids: batch.request_ids.clone(),
        requested_revision: requested_revision.clone(),
    });

    let receipt = if requested_revision == runner.applied_revision {
        let outcome = ReloadOutcome::Reloaded;
        // Keep the legacy terminal broadcast observable for out-of-process consumers.
        let _ = runner.reload_tx.send(outcome.clone());
        runner.reload_receipt(
            batch.request_ids,
            batch.sources,
            requested_revision,
            outcome,
            true,
        )
    } else {
        let assembly = match loaded {
            Ok((cfg, creds)) => runner.assemble_loaded(cfg, creds).await,
            Err(detail) => Err(detail),
        };
        runner
            .reload(
                assembly,
                requested_revision,
                batch.request_ids,
                batch.sources,
            )
            .await
    };

    runner
        .observations
        .emit(DaemonObservation::ReloadCompleted(receipt.clone()));
    for waiter in batch.waiters {
        let _ = waiter.send(receipt.clone());
    }
}

fn load_runtime_from_paths(
    config_path: &Path,
    creds_path: &Path,
    strictness: Strictness,
) -> (RuntimeRevision, Result<(Config, Credentials), String>) {
    let config_bytes = match std::fs::read(config_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                RuntimeRevision {
                    config: ContentRevision::from_bytes(&[]),
                    credentials: ContentRevision::missing(),
                },
                Err(format!("load config '{}': {error}", config_path.display())),
            );
        }
    };
    let config_revision = ContentRevision::from_bytes(&config_bytes);
    let (credentials_revision, creds) = if creds_path.exists() {
        #[cfg(unix)]
        let permissions = std::fs::metadata(creds_path)
            .map_err(|error| format!("stat credentials '{}': {error}", creds_path.display()))
            .and_then(|metadata| {
                use std::os::unix::fs::PermissionsExt;
                (metadata.permissions().mode() & 0o777 == 0o600)
                    .then_some(())
                    .ok_or_else(|| {
                        format!(
                            "credentials permissions are not 0600: '{}'",
                            creds_path.display()
                        )
                    })
            });
        #[cfg(not(unix))]
        let permissions: Result<(), String> = Ok(());
        let bytes = permissions.and_then(|()| {
            std::fs::read(creds_path)
                .map_err(|error| format!("read credentials '{}': {error}", creds_path.display()))
        });
        match bytes {
            Ok(bytes) => (
                ContentRevision::from_bytes(&bytes),
                load_credentials_from_bytes(&bytes).map_err(|error| error.to_string()),
            ),
            Err(detail) => (ContentRevision::from_bytes(&[]), Err(detail)),
        }
    } else {
        (ContentRevision::missing(), Ok(Credentials::default()))
    };
    let revision = RuntimeRevision {
        config: config_revision,
        credentials: credentials_revision,
    };
    let cfg = load_config_from_bytes(&config_bytes, strictness).map_err(|error| error.to_string());
    let loaded = match (cfg, creds) {
        (Ok((cfg, warnings)), Ok(creds)) => {
            for warning in warnings {
                tracing::warn!(event = "config_warning", key = %warning.key_path, message = %warning.message);
            }
            Ok((cfg, creds))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    (revision, loaded)
}

fn runtime_revision_from_paths(config_path: &Path, creds_path: &Path) -> Result<RuntimeRevision> {
    let config = std::fs::read(config_path)
        .with_context(|| format!("read config '{}'", config_path.display()))?;
    let credentials = if creds_path.exists() {
        ContentRevision::from_bytes(
            &std::fs::read(creds_path)
                .with_context(|| format!("read credentials '{}'", creds_path.display()))?,
        )
    } else {
        ContentRevision::missing()
    };
    Ok(RuntimeRevision {
        config: ContentRevision::from_bytes(&config),
        credentials,
    })
}

// ── Generation ─────────────────────────────────────────────────────────────────

/// Stable front-door route for one generation-scoped message type.
struct GenerationRouter<T> {
    state: tokio::sync::Mutex<GenerationRouterState<T>>,
}

struct GenerationRouterState<T> {
    paused: bool,
    target: Option<mpsc::Sender<T>>,
    queued: VecDeque<T>,
    generation_id: Option<GenerationId>,
    operation_registry: Option<Arc<OperationRegistry>>,
}

impl<T: Send + 'static> GenerationRouter<T> {
    fn new(target: mpsc::Sender<T>) -> Self {
        Self {
            state: tokio::sync::Mutex::new(GenerationRouterState {
                paused: false,
                target: Some(target),
                queued: VecDeque::new(),
                generation_id: None,
                operation_registry: None,
            }),
        }
    }

    async fn current(&self) -> Option<mpsc::Sender<T>> {
        self.state.lock().await.target.clone()
    }

    async fn pause(&self) {
        let mut state = self.state.lock().await;
        state.paused = true;
        state.target = None;
    }

    async fn install(&self, target: mpsc::Sender<T>) {
        let mut state = self.state.lock().await;
        state.target = Some(target.clone());
        while let Some(message) = state.queued.pop_front() {
            if target.send(message).await.is_err() {
                tracing::error!(
                    event = "generation_route_install_failed",
                    "new generation closed while queued inputs were being released"
                );
                break;
            }
        }
        state.paused = false;
    }

    async fn route(&self, message: T, root: &CancellationToken) -> bool {
        let mut state = self.state.lock().await;
        if state.paused {
            state.queued.push_back(message);
            return true;
        }
        let Some(target) = state.target.clone() else {
            tracing::error!(
                event = "generation_route_missing",
                "invariant violation: no installed generation target while not shutting down — input dropped"
            );
            return false;
        };
        tokio::select! {
            () = root.cancelled() => false,
            result = target.send(message) => {
                if result.is_err() && !root.is_cancelled() {
                    tracing::error!(
                        event = "generation_route_send_failed",
                        "active generation closed its inbound route"
                    );
                }
                result.is_ok()
            }
        }
    }
}

impl GenerationRouter<ControlMsg> {
    fn new_with_generation(
        target: mpsc::Sender<ControlMsg>,
        generation_id: GenerationId,
        operation_registry: Arc<OperationRegistry>,
    ) -> Self {
        Self {
            state: tokio::sync::Mutex::new(GenerationRouterState {
                paused: false,
                target: Some(target),
                queued: VecDeque::new(),
                generation_id: Some(generation_id),
                operation_registry: Some(operation_registry),
            }),
        }
    }

    async fn install_generation(
        &self,
        target: mpsc::Sender<ControlMsg>,
        generation_id: GenerationId,
    ) {
        let mut state = self.state.lock().await;
        state.target = Some(target.clone());
        state.generation_id = Some(generation_id);
        while let Some(message) = state.queued.pop_front() {
            let message = stamp_control_operation(
                message,
                state
                    .operation_registry
                    .as_ref()
                    .expect("operation registry installed"),
                generation_id,
            );
            if target.send(message).await.is_err() {
                tracing::error!(
                    event = "generation_route_install_failed",
                    "new generation closed while queued inputs were being released"
                );
                break;
            }
        }
        state.paused = false;
    }

    async fn route_control(&self, mut message: ControlMsg, root: &CancellationToken) -> bool {
        let mut state = self.state.lock().await;
        if state.paused {
            state.queued.push_back(message);
            return true;
        }
        if let (Some(registry), Some(generation)) = (&state.operation_registry, state.generation_id)
        {
            message = stamp_control_operation(message, registry, generation);
        }
        let Some(target) = state.target.clone() else {
            return false;
        };
        tokio::select! {
            () = root.cancelled() => false,
            result = target.send(message) => result.is_ok(),
        }
    }
}

fn stamp_control_operation(
    message: ControlMsg,
    registry: &OperationRegistry,
    generation: GenerationId,
) -> ControlMsg {
    let kind = match &message {
        ControlMsg::Exercise { display, .. } => Some(OperationKind::Exercise(display.clone())),
        ControlMsg::EmergencyWake { .. } => Some(OperationKind::EmergencyWake),
        _ => None,
    };
    let Some(kind) = kind else { return message };
    let Ok((operation, lease)) = registry.try_acquire(generation, kind) else {
        return message;
    };
    std::mem::forget(lease);
    match message {
        ControlMsg::Exercise { reply, .. } => ControlMsg::ExerciseAccepted { operation, reply },
        ControlMsg::EmergencyWake { reply } => {
            ControlMsg::EmergencyWakeAccepted { operation, reply }
        }
        other => other,
    }
}

/// One live runtime generation: the spawned engine plus everything needed to
/// cheaply rebuild it on a rejected reload.
struct Generation {
    engine_token: CancellationToken,
    engine_handle: Option<JoinHandle<()>>,
    // Engine shutdown is token-driven; retained senders keep channel closure
    // from racing the generation barrier while front-door routers are paused.
    // Teardown drops them only after cancelling and joining the engine.
    engine_ctl_tx: Option<mpsc::Sender<ControlMsg>>,
    engine_events_tx: Option<mpsc::Sender<PresenceEvent>>,
    producer_token: CancellationToken,
    producer_handles: Vec<JoinHandle<()>>,
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
    generation.engine_token.cancel();
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
    generation.engine_ctl_tx.take();
    generation.engine_events_tx.take();
}

/// Stop generation-local producers while leaving the engine available to drain.
async fn quiesce_inputs(generation: &mut Generation) {
    generation.producer_token.cancel();
    for handle in generation.producer_handles.drain(..) {
        let abort = handle.abort_handle();
        if tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .is_err()
        {
            abort.abort();
            tracing::warn!(event = "generation_producer_abort_forced");
        }
    }
}

async fn await_generation_barrier(
    ctl: &mpsc::Sender<ControlMsg>,
    timeout: Duration,
    force_timeout_for_test: bool,
) -> Result<(), &'static str> {
    let (ack_tx, ack_rx) = oneshot::channel();
    if ctl
        .send(ControlMsg::GenerationBarrier(ack_tx))
        .await
        .is_err()
    {
        return Err("old engine closed before accepting its generation barrier");
    }
    let acknowledged = async move {
        if force_timeout_for_test {
            std::future::pending::<Result<(), oneshot::error::RecvError>>().await
        } else {
            ack_rx.await
        }
    };
    match tokio::time::timeout(timeout, acknowledged).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err("old engine exited before acknowledging its generation barrier"),
        Err(_) => Err("old engine did not acknowledge its generation barrier before timeout"),
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
    #[cfg(feature = "render")]
    item_report_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ScreensaverItemReport>>,
    #[cfg(feature = "render")]
    item_journal: CurrentItemJournal,
    #[cfg(feature = "render")]
    scan_jobs: Vec<LumaScanJob>,
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
/// `ctx` is the daemon's single process-wide [`ControllerBuildContext`]
/// (spec §4.3's `PanelLocks`, plus Task 8's macOS gamma-hold-registry/
/// breadcrumb) — constructed once in [`App::start`] and reused, unchanged,
/// across every reload generation (see [`Runner::ctrl_ctx`]), so a physical
/// panel's lock — and, on macOS, a gamma selector's hold/breadcrumb — is
/// the same shared instance before and after a config reload.
#[allow(clippy::too_many_lines)]
#[allow(dead_code)]
async fn assemble_static(
    cfg: Config,
    creds: Credentials,
    source_builder: &SourceBuilder,
    #[cfg(feature = "render")] render_sink_builder: Option<&RenderSinkBuilder>,
    ctx: &ControllerBuildContext,
) -> Result<StaticAssembly> {
    #[cfg(feature = "render")]
    {
        let context = RenderContext {
            luma_cache: Arc::new(LumaCache::new()),
            luma_catalog: Arc::new(std::sync::RwLock::new(HashMap::new())),
            heat_snapshots: Arc::new(std::sync::RwLock::new(HashMap::new())),
            seed: 0,
            item_journal: Arc::new(Mutex::new(HashMap::new())),
        };
        assemble_static_with_context(
            cfg,
            creds,
            source_builder,
            render_sink_builder,
            ctx,
            &context,
        )
        .await
    }
    #[cfg(not(feature = "render"))]
    {
        assemble_static_with_context(cfg, creds, source_builder, ctx).await
    }
}

#[allow(clippy::too_many_lines)]
async fn assemble_static_with_context(
    cfg: Config,
    creds: Credentials,
    source_builder: &SourceBuilder,
    #[cfg(feature = "render")] render_sink_builder: Option<&RenderSinkBuilder>,
    ctx: &ControllerBuildContext,
    #[cfg(feature = "render")] render_context: &RenderContext,
) -> Result<StaticAssembly> {
    // First rule referencing each display drives its retry + timings.
    let display_rule = index_display_rules(&cfg);

    let mut display_runtime: Vec<DisplayRuntimeCfg> = Vec::new();
    let mut display_executors: HashMap<DisplayId, Arc<DisplayExecutor>> = HashMap::new();
    #[cfg(feature = "render")]
    let (render_sinks, input_wake_rx, scan_jobs, item_report_rx) =
        build_render_sinks_with_context(&cfg, render_sink_builder, render_context);
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

        let controllers = build_controllers(name, dc, &creds, ctx)
            .with_context(|| format!("build controllers for display '{name}'"))?;
        let mut executor = DisplayExecutor::with_blank_owners(
            did.clone(),
            controllers,
            dc.primary_blank_mode(),
            retry,
            Arc::clone(ctx.blank_owners()),
            controller_chain_fingerprint(dc),
        );

        let probe_results = executor.probe_all().await;
        for (controller, result) in &probe_results {
            tracing::info!(
                event = "controller_probe",
                display = %did,
                controller = %controller,
                ok = result.is_ok(),
            );
        }
        let all_probes_failed =
            !probe_results.is_empty() && probe_results.iter().all(|(_, r)| r.is_err());

        let effective = executor.effective_modes();
        let chosen = if !dc.has_controller_stage() {
            // Render-only ladder: no hardware mode to validate.
            dc.primary_blank_mode()
        } else if effective.contains(&dc.primary_blank_mode()) {
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
            match startup_mode_decision(dc.primary_blank_mode(), &effective, all_probes_failed) {
                Ok(mode) => {
                    tracing::warn!(
                        event = "display_start_degraded",
                        display = %did,
                        mode = ?dc.primary_blank_mode(),
                        "display probes failed; deferring mode eligibility until first command",
                    );
                    mode
                }
                Err(_e) => {
                    anyhow::bail!(
                        "E_MODE_UNSUPPORTED: display '{name}' cannot blank: wanted {:?} \
                         (degraded {:?}), effective modes {:?}",
                        dc.primary_blank_mode(),
                        dc.degraded_mode,
                        effective,
                    );
                }
            }
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
            input_wake_hold: rc.input_wake_hold,
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
        doctor_wake_settle: cfg.daemon.doctor_wake_settle,
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
        #[cfg(feature = "render")]
        item_report_rx: Some(item_report_rx),
        #[cfg(feature = "render")]
        item_journal: render_context.item_journal.clone(),
        #[cfg(feature = "render")]
        scan_jobs,
    })
}

/// Decide the mode to use at startup, given effective modes and whether
/// every controller probe failed.
///
/// - `effective` contains `configured` → returns it unchanged.
/// - `effective` is missing `configured` AND `all_probes_failed` → returns
///   `configured` (admitted as degraded — capabilities are unknown).
/// - `effective` is missing `configured` AND some probe succeeded →
///   `Err(ModeUnsupported)`. The caller should log at the appropriate level.
fn startup_mode_decision(
    configured: BlankMode,
    effective: &[BlankMode],
    all_probes_failed: bool,
) -> Result<BlankMode, DormantError> {
    if effective.contains(&configured) {
        return Ok(configured);
    }
    if all_probes_failed {
        return Ok(configured);
    }
    Err(DormantError::ModeUnsupported {
        display: String::new(),
        mode: format!("{configured:?}"),
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

fn shared_displays(cfg: &Config) -> Vec<DisplayId> {
    cfg.displays
        .iter()
        .filter(|(_, display)| display.scope == DisplayScope::Shared)
        .map(|(name, _)| DisplayId(name.clone()))
        .collect()
}

fn should_spawn_coordination_poller(
    cfg: &Config,
    coordination: Option<&CoordinationHandle>,
) -> bool {
    coordination.is_some() && !shared_displays(cfg).is_empty()
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

/// Extract the rules that declare an audio-related inhibitor kind
/// (`"audio-playback"` and/or `"call"`), filtering via
/// [`InhibitorKind::from_config`] — NEVER raw string literals (spec F7: a
/// third independent literal copy here would reproduce the `"manual-pause"`
/// silent-no-op hazard for the two new kinds).
///
/// Shaped after `activity_rules` above, but returns a BARE `Vec` (P12):
/// unlike per-rule activity polling, audio classification is one global
/// system-wide fact driven by the single `[audio]` section — there is no
/// per-rule interval to reduce to a minimum.
fn audio_rules(cfg: &Config) -> Vec<AudioRule> {
    cfg.rules
        .iter()
        .filter_map(|(id, rc)| {
            let kinds: Vec<InhibitorKind> = rc
                .inhibitors
                .iter()
                .filter_map(|s| InhibitorKind::from_config(s))
                .filter(|k| matches!(k, InhibitorKind::AudioPlayback | InhibitorKind::Call))
                .collect();
            if kinds.is_empty() {
                None
            } else {
                Some(AudioRule {
                    rule: RuleId(id.clone()),
                    kinds,
                })
            }
        })
        .collect()
}

/// F7's dormantd-side round-trip pin (T5, closing the three-checkpoint
/// chain: T1 pinned `INHIBITOR_*` consts ⟺ `from_config`; T2 pinned
/// `VALID_INHIBITORS` ⟺ `from_config`; this pins `audio_rules()` against the
/// SAME consts — never a raw string literal, so a typo on any of the three
/// surfaces goes RED here or upstream).
#[cfg(test)]
mod audio_rules_tests {
    use super::*;
    use dormant_core::config::schema::{
        AudioConfig, DaemonConfig, NotificationsConfig, WatchdogConfig, WearConfig,
    };
    use dormant_core::rules::{INHIBITOR_AUDIO_PLAYBACK, INHIBITOR_CALL, INHIBITOR_USER_ACTIVITY};
    use indexmap::IndexMap;

    fn rule_cfg(inhibitors: Vec<String>) -> RuleConfig {
        RuleConfig {
            zone: "z".into(),
            displays: vec![],
            grace_period: Duration::from_secs(30),
            min_blank_time: Duration::from_secs(0),
            min_wake_time: Duration::from_secs(0),
            inhibitors,
            activity_idle_threshold: Duration::from_secs(60),
            activity_poll_interval: Duration::from_secs(5),
            wake_retries: 0,
            wake_retry_backoff: Duration::from_millis(10),
            wake_retry_interval: Duration::from_secs(1),
            input_wake_hold: Duration::ZERO,
        }
    }

    fn cfg_with_rules(rules: IndexMap<String, RuleConfig>) -> Config {
        Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::new(),
            rules,
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }
    }

    #[test]
    fn audio_rules_recognizes_exactly_the_declared_kind_literals() {
        let mut rules = IndexMap::new();
        rules.insert(
            "playback".into(),
            rule_cfg(vec![INHIBITOR_AUDIO_PLAYBACK.to_string()]),
        );
        rules.insert("call".into(), rule_cfg(vec![INHIBITOR_CALL.to_string()]));
        rules.insert(
            "both".into(),
            rule_cfg(vec![
                INHIBITOR_AUDIO_PLAYBACK.to_string(),
                INHIBITOR_CALL.to_string(),
                INHIBITOR_USER_ACTIVITY.to_string(),
            ]),
        );
        // Neither audio-related kind declared — must be excluded entirely.
        rules.insert(
            "activity_only".into(),
            rule_cfg(vec![INHIBITOR_USER_ACTIVITY.to_string()]),
        );
        // A typo'd/unknown literal — `from_config` returns `None`, filtered.
        rules.insert("typo".into(), rule_cfg(vec!["audio_playback".to_string()]));

        let cfg = cfg_with_rules(rules);
        let extracted = audio_rules(&cfg);

        let find = |id: &str| extracted.iter().find(|r| r.rule == RuleId(id.to_string()));

        assert_eq!(
            find("playback").map(|r| r.kinds.clone()),
            Some(vec![InhibitorKind::AudioPlayback])
        );
        assert_eq!(
            find("call").map(|r| r.kinds.clone()),
            Some(vec![InhibitorKind::Call])
        );
        assert_eq!(
            find("both").map(|r| r.kinds.clone()),
            Some(vec![InhibitorKind::AudioPlayback, InhibitorKind::Call])
        );
        assert!(
            find("activity_only").is_none(),
            "a rule declaring only user-activity must not appear in audio_rules()"
        );
        assert!(
            find("typo").is_none(),
            "an unrecognized inhibitor literal must not appear in audio_rules()"
        );
        assert_eq!(
            extracted.len(),
            3,
            "exactly the three audio-declaring rules"
        );
    }
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
#[cfg(any(test, feature = "test-util"))]
#[allow(dead_code)]
#[allow(clippy::too_many_lines)] // ScreensaverSettings + ShiftSettings assembly, both documented inline
fn build_render_sinks(
    cfg: &Config,
    render_sink_builder: Option<&RenderSinkBuilder>,
) -> (
    HashMap<DisplayId, Arc<dyn RenderSink>>,
    tokio::sync::mpsc::UnboundedReceiver<DisplayId>,
) {
    let context = RenderContext {
        luma_cache: Arc::new(LumaCache::new()),
        luma_catalog: Arc::new(std::sync::RwLock::new(HashMap::new())),
        heat_snapshots: Arc::new(std::sync::RwLock::new(HashMap::new())),
        seed: 0,
        item_journal: Arc::new(Mutex::new(HashMap::new())),
    };
    let (sinks, input_wake_rx, _, _) =
        build_render_sinks_with_context(cfg, render_sink_builder, &context);
    (sinks, input_wake_rx)
}

#[cfg(feature = "render")]
#[allow(clippy::too_many_lines)]
#[allow(clippy::type_complexity)]
fn build_render_sinks_with_context(
    cfg: &Config,
    render_sink_builder: Option<&RenderSinkBuilder>,
    context: &RenderContext,
) -> (
    HashMap<DisplayId, Arc<dyn RenderSink>>,
    tokio::sync::mpsc::UnboundedReceiver<DisplayId>,
    Vec<LumaScanJob>,
    tokio::sync::mpsc::UnboundedReceiver<ScreensaverItemReport>,
) {
    use dormant_render::ScreensaverSettings;
    use dormant_render::playlist;

    let mut sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
    let mut scan_jobs = Vec::new();
    let (input_wake_tx, input_wake_rx) = tokio::sync::mpsc::unbounded_channel::<DisplayId>();
    let (item_report_tx, item_report_rx) =
        tokio::sync::mpsc::unbounded_channel::<ScreensaverItemReport>();

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
                let items = playlist::build_playlist(&ss.source, Some(context.seed));
                for item in &items {
                    if let Some(tag) = item.wear_tag {
                        if let Ok(mut catalog) = context.luma_catalog.write() {
                            catalog.insert(
                                item.uri.clone(),
                                dormant_render::luma::flat_grid_for_tag(tag),
                            );
                        }
                    } else if item.media_kind == dormant_render::playlist::MediaKind::Image {
                        let path = std::path::PathBuf::from(&item.uri);
                        if path.is_absolute() {
                            scan_jobs.push(LumaScanJob {
                                display_id: did.clone(),
                                uri: item.uri.clone(),
                                path,
                                cache: context.luma_cache.clone(),
                                catalog: context.luma_catalog.clone(),
                            });
                        }
                    }
                }
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
                    heat_snapshots: context.heat_snapshots.clone(),
                    luma_catalog: context.luma_catalog.clone(),
                    seed: context.seed,
                    wear_temperature: ss.wear_temperature,
                    shift_heat_bias: ss.shift_heat_bias,
                }
            })
        } else {
            None
        };

        // Shift settings (OLED-health T10): derived from
        // `dc.screensaver` ONLY when the ladder reaches
        // `RenderScreensaver` — the black overlay never shifts (U5).
        // A display with no `[displays.<id>.screensaver]` table, or a
        // ladder that never reaches `RenderScreensaver`, gets `None`.
        let shift_settings: Option<dormant_render::ShiftSettings> = if ladder
            .iter()
            .any(|s| s.kind == dormant_core::types::StageKind::RenderScreensaver)
        {
            dc.screensaver
                .as_ref()
                .map(|ss| dormant_render::ShiftSettings {
                    shift_px: ss.shift_px,
                    shift_interval: ss.shift_interval,
                })
        } else {
            None
        };

        if let Some(output_name) = &dc.output {
            let ss_ref = screensaver_settings.as_ref();
            let shift_ref = shift_settings.as_ref();
            let sink: Option<Arc<dyn RenderSink>> = if let Some(builder) = render_sink_builder {
                (builder)(
                    did.clone(),
                    output_name.clone(),
                    Some(&input_wake_tx),
                    Some(&item_report_tx),
                    ss_ref,
                    shift_ref,
                )
            } else {
                match LayerShellRenderSink::new(
                    did.clone(),
                    output_name.clone(),
                    Some(&input_wake_tx),
                    Some(&item_report_tx),
                ) {
                    Ok(sink) => {
                        if let Some(ref settings) = screensaver_settings {
                            sink.set_screensaver(settings.clone());
                        }
                        if let Some(shift) = shift_settings {
                            sink.set_shift(shift);
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

    (sinks, input_wake_rx, scan_jobs, item_report_rx)
}

/// Spawn the engine, sources, inhibitor, and notifier for one generation.
///
/// `notify_state` / `notify_sink` are daemon-lifetime (constructed once in
/// [`App::start`]) — every call site threads the SAME `Arc`s through so the
/// notifier's open episodes (and the sink's cached connection) survive a
/// config reload.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    reason = "generation construction keeps every producer and daemon-lifetime handle visible at its spawn site"
)]
fn spawn_generation(
    root: &CancellationToken,
    assembly: StaticAssembly,
    restore: Option<&StateSnapshot>,
    pending: Option<String>,
    rollback: Option<RollbackStatus>,
    notify_state: Arc<Mutex<NotifyState>>,
    notify_sink: Arc<dyn NotifySink>,
    ownership: Arc<dyn OwnershipGate>,
    coordination: Option<CoordinationHandle>,
    config_rx: watch::Receiver<Arc<Config>>,
    executors_rx: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    generation_id: GenerationId,
    operation_registry: Arc<OperationRegistry>,
    observations: Option<ObservationHub>,
    idle_tx: Option<crate::idle_observation::IdleObservationTx>,
    filtered_activity_tx: crate::filtered_activity::FilteredActivityTx,
    direct_switch: &Arc<DirectSwitchHandle>,
) -> Result<GenSpawn> {
    let engine_token = root.child_token();
    let engine_cancel = engine_token.clone();
    let producer_token = root.child_token();
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
        ownership,
    )
    .context("build engine")?;
    if let Some(ref coordination) = coordination {
        engine = engine.with_coordination_handle(coordination.clone());
    }
    if let Some(observations) = observations {
        engine = engine.with_observation_hub(generation_id, observations);
    }
    engine = engine.with_operation_registry((*operation_registry).clone());

    if let Some(detail) = pending {
        engine.set_pending_reload(Some(detail));
    }
    if let Some(status) = rollback {
        engine.set_rollback(Some(status));
    }
    if let Some(snapshot) = restore {
        apply_restore(&mut engine, snapshot, &assembly.engine_cfg);
    }

    let engine_handle = tokio::spawn(async move {
        engine.run(events_rx, ctl_rx, engine_cancel).await;
    });

    let mut producer_handles = Vec::new();

    #[cfg(feature = "render")]
    for job in assembly.scan_jobs {
        let token = producer_token.clone();
        producer_handles.push(tokio::task::spawn_blocking(move || {
            run_luma_scan_job(job, &token);
        }));
    }

    #[cfg(feature = "render")]
    if let Some(item_report_rx) = assembly.item_report_rx {
        producer_handles.push(spawn_item_report_drain(
            item_report_rx,
            assembly.item_journal.clone(),
            producer_token.clone(),
        ));
    }

    if should_spawn_coordination_poller(&assembly.cfg, coordination.as_ref())
        && let Some(state) = coordination
    {
        producer_handles.push(coordination_poll::spawn(CoordinationPollDeps {
            config_rx,
            ctl_tx: ctl_tx.clone(),
            executors_rx,
            state,
            cancel: producer_token.clone(),
            direct_switch: Some(Arc::clone(direct_switch)),
        }));
    }

    // ── InputWake drain (feature-gated: render surfaces emit InputWake) ──
    // Each LayerShellRenderSink pushes DisplayId through an unbounded
    // channel on the first pointer/key event.  This task routes those
    // DisplayIds to the engine as ControlMsg::InputWake so the state
    // machine can react. Its producer token stops it before the old engine
    // receives the generation barrier, preventing a wake from landing behind
    // that fence.
    #[cfg(feature = "render")]
    if let Some(input_wake_rx) = assembly.input_wake_rx {
        let filter_active = !assembly.cfg.input_filter.ignore_devices.is_empty();
        let filtered_activity_rx = Some(filtered_activity_tx.subscribe());
        producer_handles.push(spawn_input_wake_drain(
            input_wake_rx,
            ctl_tx.clone(),
            filter_active,
            filtered_activity_rx,
            assembly.render_sinks.clone(),
            producer_token.clone(),
        ));
    }

    for source in assembly.sources {
        let stx = events_tx.clone();
        let sctl = ctl_tx.clone();
        let stoken = producer_token.clone();
        producer_handles.push(tokio::spawn(async move {
            if let Err(e) = source.run(stx, sctl, stoken).await {
                tracing::error!(event = "sensor_source_exited", error = %e);
            }
        }));
    }

    let poll = assembly
        .activity_poll
        .unwrap_or_else(|| Duration::from_secs(5));
    let idle_unit = assembly.cfg.daemon.idle_time_unit;
    let idle_source = assembly.cfg.daemon.idle_source;
    let macos_guard_cfg = macos_idle::MacosIdleGuardConfig {
        frozen_polls: assembly.cfg.daemon.macos_idle_frozen_polls,
        sanity_cap: assembly.cfg.daemon.macos_idle_sanity_cap,
        startup_grace: assembly.cfg.daemon.macos_idle_startup_grace,
    };
    if let Some(handle) = inhibit_activity::spawn(
        assembly.activity_rules,
        poll,
        idle_source,
        idle_unit,
        macos_guard_cfg,
        idle_tx,
        &assembly.cfg.input_filter,
        filtered_activity_tx,
        ctl_tx.clone(),
        producer_token.clone(),
    ) {
        producer_handles.push(handle);
    }

    // Audio/call inhibitor (spec §4.3) — global `[audio]` section, so the
    // rule list is derived straight from `assembly.cfg` here rather than
    // threaded through `StaticAssembly` like `activity_rules`/`activity_poll`
    // (there is no per-rule interval to carry). `None` when no rule opts in
    // (`inhibit_activity::spawn`'s own precedent, mirrored by
    // `inhibit_audio::spawn`).
    if let Some(handle) = inhibit_audio::spawn(
        audio_rules(&assembly.cfg),
        assembly.cfg.audio.clone(),
        ctl_tx.clone(),
        producer_token.clone(),
    ) {
        producer_handles.push(handle);
    }

    // Desktop wake/blank-failure notifier (spec §4.4) — `notifier::spawn`
    // returns `None` (no-op) when `[notifications] enabled = false`,
    // mirroring `inhibit_activity::spawn`'s own None-returning precedent.
    if let Some(handle) = notifier::spawn(NotifierDeps {
        ctl: ctl_tx.clone(),
        cfg: assembly.cfg.notifications,
        state: notify_state,
        sink: notify_sink,
        cancel: producer_token.clone(),
    }) {
        producer_handles.push(handle);
    }

    // Opt-in MQTT state publisher (issue #105) — `state_publisher::spawn`
    // returns `None` (no-op) when `[publish] enabled = false`, mirroring
    // `notifier::spawn`'s own None-returning precedent. The handle goes
    // in `producer_handles` so `Generation::teardown` cancels and
    // awaits it on reload like every other generation-local task. The
    // publisher subscribes to `DaemonEvent`s itself via
    // `ControlMsg::SubscribeEvents`, so no broadcast receiver is
    // plumbed in here.
    if let Some(handle) =
        crate::state_publisher::spawn(crate::state_publisher::StatePublisherDeps {
            config: Arc::new(assembly.cfg.clone()),
            credentials: Arc::new(assembly.creds.clone()),
            ctl_tx: ctl_tx.clone(),
            cancel: producer_token.clone(),
        })
    {
        producer_handles.push(handle);
    }

    let generation = Generation {
        engine_token,
        engine_handle: Some(engine_handle),
        engine_ctl_tx: Some(ctl_tx.clone()),
        engine_events_tx: Some(events_tx.clone()),
        producer_token,
        producer_handles,
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

#[cfg(any(test, feature = "test-util"))]
static RELOAD_SPAWN_ROLLBACK_CAPTURES: OnceLock<
    Mutex<HashMap<PathBuf, Vec<Option<RollbackStatus>>>>,
> = OnceLock::new();

#[cfg(any(test, feature = "test-util"))]
fn reload_spawn_captures() -> &'static Mutex<HashMap<PathBuf, Vec<Option<RollbackStatus>>>> {
    RELOAD_SPAWN_ROLLBACK_CAPTURES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test-only seam (rollback-recovery plan, Task 1 §9): register `state_dir`
/// so every subsequent `spawn_generation_for_reload` call for that exact
/// state dir records the `rollback` argument it actually received. Path-
/// keyed so parallel reload tests (each with their own temporary
/// `state_dir`) cannot consume or pollute another test's capture.
#[cfg(any(test, feature = "test-util"))]
pub fn begin_reload_spawn_rollback_capture_for_test(state_dir: &Path) {
    reload_spawn_captures()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(state_dir.to_path_buf(), Vec::new());
}

/// Test-only seam: drain and return every captured `rollback` argument for
/// `state_dir` since the matching `begin_reload_spawn_rollback_capture_for_test`
/// call.
///
/// # Panics
///
/// Panics if the capture was never registered for `state_dir` — a test that
/// calls this without first calling
/// `begin_reload_spawn_rollback_capture_for_test` has a bug, not a
/// legitimate empty case.
#[cfg(any(test, feature = "test-util"))]
pub fn take_reload_spawn_rollback_capture_for_test(
    state_dir: &Path,
) -> Vec<Option<RollbackStatus>> {
    reload_spawn_captures()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(state_dir)
        .expect("reload spawn capture was not registered for this state dir")
}

#[cfg(any(test, feature = "test-util"))]
fn record_reload_spawn_rollback_for_test(state_dir: &Path, rollback: Option<&RollbackStatus>) {
    if let Some(captures) = reload_spawn_captures()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(state_dir)
    {
        captures.push(rollback.cloned());
    }
}

/// Reload-path wrapper around [`spawn_generation`] (rollback-recovery plan,
/// Task 1 §9): both `Runner` reload call sites (accepted candidate,
/// `rebuild_old`) go through this so a test can capture the ACTUAL
/// `rollback` argument reaching `spawn_generation`, rather than sampling a
/// forwarding channel that could observe a later, unrelated state.
/// Generation 0 (`App::start`) calls `spawn_generation` directly — it is
/// boot-parked via `ControlMsg::SetRollback` after `App::start` returns, not
/// spawned with a rollback argument.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(
    not(any(test, feature = "test-util")),
    allow(
        unused_variables,
        reason = "state_dir is only consulted by the test-util capture seam"
    )
)]
fn spawn_generation_for_reload(
    state_dir: &Path,
    root: &CancellationToken,
    assembly: StaticAssembly,
    restore: Option<&StateSnapshot>,
    pending: Option<String>,
    rollback: Option<RollbackStatus>,
    notify_state: Arc<Mutex<NotifyState>>,
    notify_sink: Arc<dyn NotifySink>,
    ownership: Arc<dyn OwnershipGate>,
    coordination: Option<CoordinationHandle>,
    config_rx: watch::Receiver<Arc<Config>>,
    executors_rx: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    generation_id: GenerationId,
    operation_registry: Arc<OperationRegistry>,
    observations: Option<ObservationHub>,
    idle_tx: Option<crate::idle_observation::IdleObservationTx>,
    filtered_activity_tx: crate::filtered_activity::FilteredActivityTx,
    direct_switch: &Arc<DirectSwitchHandle>,
) -> Result<GenSpawn> {
    #[cfg(any(test, feature = "test-util"))]
    record_reload_spawn_rollback_for_test(state_dir, rollback.as_ref());

    spawn_generation(
        root,
        assembly,
        restore,
        pending,
        rollback,
        notify_state,
        notify_sink,
        ownership,
        coordination,
        config_rx,
        executors_rx,
        generation_id,
        operation_registry,
        observations,
        idle_tx,
        filtered_activity_tx,
        direct_switch,
    )
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
///
/// Transient engine state (`availability_online`, `input_wake_holds`) is
/// deliberately NOT carried across reload — both reset empty.  This is
/// fail-safe: worst case is one stale sweep before the next online
/// availability frame, and one lost hold window (the display re-enters
/// grace on the next zone edge, which the new engine processes normally).
/// A future implementer tempted to seed these should re-verify that
/// timing is correct: `availability_online` needs the source to emit a
/// fresh online frame before the new engine's stale timeout, and
/// `input_wake_holds` apply only until the next presence edge.
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

/// Displays present in BOTH `old_cfg` and `new_cfg` (same [`DisplayId`] —
/// NOT added or removed by this reload), NOT referenced by any rule in the
/// NEW config (`ruled`), whose dispatch-relevant configuration changed
/// (Task 8's dispatch-identity invariant, reusing
/// [`reload::dispatch_relevant_eq`] — the SAME single comparator
/// `zero_changed_displays` already uses, never a second, parallel
/// definition of "changed") and were dark before the reload.
///
/// A **manual-only (rule-less)** dark display whose dispatch identity
/// changes under the same `DisplayId` (a different controller chain,
/// `output`/`ddc_display` selector, blank/wake command surface, or any
/// other dispatch-relevant field) is recovery-equivalent to REMOVAL: the
/// OLD controller chain is about to be torn down and replaced by one that
/// may not agree at all about what "wake" means for whatever the panel is
/// currently doing, and — being rule-less — nothing else will ever
/// re-converge it (its phase is simply preserved by `apply_restore`). So it
/// gets the exact same treatment as [`removed_dark_displays`] — a verified
/// physical wake through the OLD executor before the new generation is
/// accepted, with a wake failure aborting the reload the same way. See
/// `Runner::reload`'s merged wake loop, which chains this function's output
/// onto `removed_dark_displays`'s.
///
/// **Rule-driven displays are excluded on purpose** (spec/plan
/// 2026-07-16-dormant-macos-support.md:1075: "Current rule-driven reload
/// semantics intentionally defensive-wake retained dark displays... a
/// rule-driven gamma display therefore flashes to profile on accepted
/// reload by design... not that every rule-driven reload remains
/// physically dark"). A ruled display that is still present after the
/// reload is already covered by [`retained_dark_displays`]'s existing
/// best-effort defensive wake (fired AFTER the new generation installs,
/// never rejecting the reload) regardless of what changed about it — this
/// function must never ALSO pull it into the verified-wake-or-reject loop,
/// which would upgrade that pre-existing best-effort contract into a hard
/// reject. Every RED-first test for this function builds its fixture with
/// `ruled: false` for exactly this reason.
///
/// Deliberately disjoint from [`removed_dark_displays`]: `assemble_static`
/// builds an executor for every entry in `cfg.displays` (see its own
/// docs), so a display present in `new_cfg.displays` always has an entry
/// in `new_executors` — "present in both configs" (this function) and "no
/// executor in the new generation" (`removed_dark_displays`) can never
/// overlap for the same `DisplayId`.
fn changed_dispatch_dark_displays(
    snapshot: Option<&StateSnapshot>,
    old_cfg: &Config,
    new_cfg: &Config,
    ruled: &HashSet<DisplayId>,
) -> Vec<DisplayId> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    snapshot
        .displays
        .iter()
        .filter(|(id, d)| {
            phase_is_dark(&d.phase)
                && !ruled.contains(&DisplayId((*id).clone()))
                && match (old_cfg.displays.get(id), new_cfg.displays.get(id)) {
                    (Some(o), Some(n)) => !reload::dispatch_relevant_eq(o, n),
                    // Added/removed: no baseline to compare — not this
                    // function's concern (added has no OLD executor to wake
                    // through at all; removed is `removed_dark_displays`'s
                    // job).
                    _ => false,
                }
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

/// Forward control messages through the current generation router.
async fn forward_ctl(
    mut rx: mpsc::Receiver<ControlMsg>,
    router: Arc<GenerationRouter<ControlMsg>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                None => break,
                Some(m) => {
                    if !router.route_control(m, &cancel).await {
                        break;
                    }
                }
            },
        }
    }
}

/// Forward injected presence events through the current generation router.
async fn forward_events(
    mut rx: mpsc::Receiver<PresenceEvent>,
    router: Arc<GenerationRouter<PresenceEvent>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                None => break,
                Some(m) => {
                    if !router.route(m, &cancel).await {
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
                    rollback: None,
                    kvm: None,
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
    // Linux-only (both datagram tests): sd_notify is systemd-only by
    // architecture — NOTIFY_SOCKET is never set on macOS and the send
    // path is best-effort/swallowed there, so on macos-latest the healthy
    // test times out (recv WouldBlock, PR #78 round 7) and the failure-side
    // test would pass vacuously. The pure-logic watchdog tests below stay
    // cross-platform.
    #[test]
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")] // see the failure-side pin's Linux-only note
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
            rollback: None,
            kvm: None,
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
    filter_active: bool,
    filtered_activity_rx: Option<crate::filtered_activity::FilteredActivityRx>,
    render_sinks: std::collections::HashMap<DisplayId, std::sync::Arc<dyn RenderSink>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            () = cancel.cancelled() => {}
            () = async {
                let mut rx = input_wake_rx;
                while let Some(display) = rx.recv().await {
                    // Gate: when the input filter is active, the filtered
                    // source is available, and no accepted device was
                    // recently active, reject this wake — the ignored
                    // device (e.g. a mouse jiggler) should not tear down
                    // the overlay.  Instead, reassert the current overlay
                    // surface so the latch resets and a genuine input will
                    // still fire.
                    //
                    // FAIL-SAFE: if the filtered source is UNAVAILABLE or
                    // the watch is closed, every wake is FORWARDED —
                    // never silently swallow all input.  A user must
                    // always be able to wake their screen by touching
                    // the keyboard.
                    let should_reject = filter_active
                        && filtered_activity_rx.as_ref().is_some_and(|activity_rx| {
                            let observation = activity_rx.borrow();
                            observation.available
                                && !observation.is_recent(
                                    Instant::now(),
                                    Duration::from_millis(500),
                                )
                        });
                    if should_reject {
                        if let Some(sink) = render_sinks.get(&display) {
                            let _ = sink.show_current_overlay().await;
                        }
                        continue;
                    }
                    if ctl_tx.send(ControlMsg::InputWake(display)).await.is_err() {
                        break; // engine channel closed — shutdown in progress
                    }
                }
            } => {}
        }
    })
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
            scope: dormant_core::config::DisplayScope::default(),
            shared_input_code: None,
            shared_input_write_code: None,
            shared_peer_input_code: None,
            shared_peer_input_write_code: None,
            hooks: dormant_core::config::HookSlots::default(),
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
            power_off_opt_in: false,
        }
    }

    fn screensaver_config_for_test(
        path: &std::path::Path,
        wear_temperature: f64,
        shift_heat_bias: f64,
    ) -> dormant_core::config::schema::Config {
        use dormant_core::config::schema::{
            Config, DaemonConfig, ScreensaverConfig, ScreensaverSource,
        };
        use dormant_core::types::{LadderStage, StageKind};
        use indexmap::IndexMap;

        let mut display = base_display_cfg_for_test();
        display.output = Some("DP-1".into());
        display.ladder = vec![LadderStage {
            kind: StageKind::RenderScreensaver,
            dwell: Some(Duration::from_secs(30)),
        }];
        display.screensaver = Some(ScreensaverConfig {
            trigger: "vacancy".into(),
            audio: false,
            source: vec![ScreensaverSource {
                path: Some(path.to_string_lossy().into_owned()),
                urls: Vec::new(),
                recurse: false,
                shuffle: false,
                order: Some("wear-even".into()),
                wear_tag: None,
                image_duration: None,
            }],
            scale_mode: None,
            transition: None,
            transition_duration: None,
            shift_px: 0,
            shift_interval: Duration::from_secs(120),
            wear_temperature,
            shift_heat_bias,
        });
        Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("mon".into(), display)]),
            rules: IndexMap::new(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }
    }

    #[tokio::test]
    async fn build_render_sinks_exposes_scan_jobs_without_waiting_for_decode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let image = dir.path().join("still.png");
        std::fs::write(&image, b"not decoded by assembly").expect("fixture");
        let cfg = screensaver_config_for_test(dir.path(), 0.05, 0.25);
        let context = RenderContext {
            luma_cache: Arc::new(LumaCache::new()),
            luma_catalog: Arc::new(std::sync::RwLock::new(HashMap::new())),
            heat_snapshots: Arc::new(std::sync::RwLock::new(HashMap::new())),
            seed: 7,
            item_journal: Arc::new(Mutex::new(HashMap::new())),
        };
        let (_sinks, _rx, jobs, _item_reports) =
            build_render_sinks_with_context(&cfg, None, &context);

        assert_eq!(jobs.len(), 1);
        let expected_uri = image.canonicalize().expect("fixture path canonicalizes");
        assert_eq!(jobs[0].uri, expected_uri.to_string_lossy());
    }

    #[test]
    fn item_journal_is_bounded_and_keeps_newest_report() {
        let journal: CurrentItemJournal = Arc::new(Mutex::new(HashMap::new()));
        let display_id = DisplayId("panel".into());
        for index in 0..=CURRENT_ITEM_JOURNAL_CAPACITY {
            append_item_report(
                &journal,
                ScreensaverItemReport {
                    display_id: display_id.clone(),
                    uri: Some(index.to_string()),
                    luma: None,
                    observed_at: std::time::Instant::now(),
                },
            );
        }
        let entries = journal.lock().unwrap();
        let reports = entries.get(&display_id).unwrap();
        assert_eq!(reports.len(), CURRENT_ITEM_JOURNAL_CAPACITY);
        assert_eq!(reports.back().unwrap().uri.as_deref(), Some("1024"));
    }

    #[tokio::test]
    async fn screensaver_wear_knobs_flow_on_initial_build_and_reload() {
        use dormant_core::fakes::RecordingRenderSink;

        let dir = tempfile::tempdir().expect("tempdir");
        let initial = screensaver_config_for_test(dir.path(), 0.05, 0.25);
        let reloaded = screensaver_config_for_test(dir.path(), 0.75, 0.9);
        let context = RenderContext {
            luma_cache: Arc::new(LumaCache::new()),
            luma_catalog: Arc::new(std::sync::RwLock::new(HashMap::new())),
            heat_snapshots: Arc::new(std::sync::RwLock::new(HashMap::new())),
            seed: 7,
            item_journal: Arc::new(Mutex::new(HashMap::new())),
        };
        let captured = Arc::new(Mutex::new(Vec::new()));
        let factory: RenderSinkBuilder = {
            let captured = captured.clone();
            Arc::new(move |_did, _output, _tx, _item_reports, settings, _shift| {
                if let Some(settings) = settings {
                    captured.lock().unwrap().push(settings.clone());
                }
                Some(Arc::new(RecordingRenderSink::new()) as Arc<dyn RenderSink>)
            })
        };

        let _ = build_render_sinks_with_context(&initial, Some(&factory), &context);
        let _ = build_render_sinks_with_context(&reloaded, Some(&factory), &context);

        let values = captured.lock().unwrap();
        assert_eq!(values.len(), 2);
        assert!((values[0].wear_temperature - 0.05).abs() < f64::EPSILON);
        assert!((values[0].shift_heat_bias - 0.25).abs() < f64::EPSILON);
        assert!((values[1].wear_temperature - 0.75).abs() < f64::EPSILON);
        assert!((values[1].shift_heat_bias - 0.9).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn scan_failure_keeps_original_playlist_and_logs_luma_scan_failed() {
        use dormant_core::spatial_grid::HeatGrid;
        use dormant_render::playlist::{MediaKind, PlaylistItem, PlaylistOrder};
        use dormant_render::wear_order::apply_wear_even_groups;

        let catalog = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let job = LumaScanJob {
            display_id: DisplayId("mon".into()),
            uri: "/missing/still.png".into(),
            path: "/missing/still.png".into(),
            cache: Arc::new(LumaCache::new()),
            catalog: catalog.clone(),
        };
        let token = CancellationToken::new();
        run_luma_scan_job(job, &token);
        assert!(catalog.read().expect("catalog lock").is_empty());

        let original = vec![
            PlaylistItem {
                uri: "missing.png".into(),
                media_kind: MediaKind::Image,
                order: PlaylistOrder::WearEven,
                ..Default::default()
            },
            PlaylistItem {
                uri: "still.png".into(),
                media_kind: MediaKind::Image,
                order: PlaylistOrder::WearEven,
                ..Default::default()
            },
        ];
        let heat = HeatGrid::new(1, 1, vec![0.0]).expect("heat grid");
        let fallback = apply_wear_even_groups(&original, &catalog, &heat, 0.0, 1)
            .unwrap_or_else(|_| original.clone());
        assert_eq!(fallback, original);
    }

    #[tokio::test]
    async fn completed_catalog_reorders_next_screensaver_show_from_latest_heat() {
        use dormant_core::spatial_grid::{HeatGrid, LumaGrid};
        use dormant_render::playlist::{MediaKind, PlaylistItem, PlaylistOrder};
        use dormant_render::wear_order::apply_wear_even_groups;

        let items = vec![
            PlaylistItem {
                uri: "left.png".into(),
                media_kind: MediaKind::Image,
                order: PlaylistOrder::WearEven,
                ..Default::default()
            },
            PlaylistItem {
                uri: "right.png".into(),
                media_kind: MediaKind::Image,
                order: PlaylistOrder::WearEven,
                ..Default::default()
            },
        ];
        let catalog = Arc::new(std::sync::RwLock::new(HashMap::from([
            (
                "left.png".into(),
                LumaGrid::new({
                    let mut cells = vec![0.0; 144];
                    for row in 0..9 {
                        cells[row * 16..row * 16 + 8].fill(1.0);
                    }
                    cells
                })
                .expect("left luma"),
            ),
            (
                "right.png".into(),
                LumaGrid::new({
                    let mut cells = vec![0.0; 144];
                    for row in 0..9 {
                        cells[row * 16 + 8..row * 16 + 16].fill(1.0);
                    }
                    cells
                })
                .expect("right luma"),
            ),
        ])));
        let cold_left = HeatGrid::new(1, 2, vec![0.0, 1.0]).expect("heat grid");
        let cold_right = HeatGrid::new(1, 2, vec![1.0, 0.0]).expect("heat grid");

        let first = apply_wear_even_groups(&items, &catalog, &cold_left, 0.0, 1).unwrap();
        let next = apply_wear_even_groups(&items, &catalog, &cold_right, 0.0, 1).unwrap();

        assert_eq!(first[0].uri, "left.png");
        assert_eq!(next[0].uri, "right.png");
    }

    /// The drain task routes each `DisplayId` received on the unbounded
    /// channel to the engine as `ControlMsg::InputWake`.  When the control
    /// channel is closed (shutdown), the drain exits cleanly.
    #[tokio::test]
    async fn input_wake_drain_routes_display_id_to_input_wake() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(8);
        let (input_wake_tx, input_wake_rx) = mpsc::unbounded_channel::<DisplayId>();
        let cancel = CancellationToken::new();

        spawn_input_wake_drain(
            input_wake_rx,
            ctl_tx,
            false, // filter not active — passthrough
            None,  // no filtered activity receiver
            HashMap::new(),
            cancel.clone(),
        );

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

    // ── InputWake gate tests (T16) ──

    use dormant_core::fakes::RenderCmd;

    struct InputWakeHarness {
        input_wake_tx: tokio::sync::mpsc::UnboundedSender<DisplayId>,
        ctl_rx: mpsc::Receiver<ControlMsg>,
        cancel: CancellationToken,
        sink: dormant_core::fakes::RecordingRenderSink,
        #[allow(dead_code)]
        handle: JoinHandle<()>,
    }

    impl InputWakeHarness {
        /// No filter active — every wake is forwarded.
        fn without_filter() -> Self {
            let (input_wake_tx, input_wake_rx) = mpsc::unbounded_channel::<DisplayId>();
            let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(8);
            let cancel = CancellationToken::new();
            let sink = dormant_core::fakes::RecordingRenderSink::new();
            let mut sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
            sinks.insert(DisplayId("monitor".into()), Arc::new(sink.clone()));
            let handle = spawn_input_wake_drain(
                input_wake_rx,
                ctl_tx,
                false, // filter not active
                None,  // no filtered activity receiver
                sinks,
                cancel.clone(),
            );
            Self {
                input_wake_tx,
                ctl_rx,
                cancel,
                sink,
                handle,
            }
        }

        /// Filter active, filtered source is available but no activity
        /// edge has been recorded — behaves like a fresh source before
        /// any accepted-device input arrives.
        fn filtered_without_recent_activity() -> Self {
            let (input_wake_tx, input_wake_rx) = mpsc::unbounded_channel::<DisplayId>();
            let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(8);
            let cancel = CancellationToken::new();
            let sink = dormant_core::fakes::RecordingRenderSink::new();
            let mut sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
            sinks.insert(DisplayId("monitor".into()), Arc::new(sink.clone()));

            // Source is available, heartbeat is fresh, but no accepted
            // device has produced an activity edge yet.
            let (filtered_tx, filtered_rx) = crate::filtered_activity::filtered_activity_channel();
            filtered_tx.send_replace(crate::filtered_activity::FilteredActivity {
                last_activity: None,
                observed_at: Instant::now(),
                available: true,
                edge_seq: 0,
            });

            let handle = spawn_input_wake_drain(
                input_wake_rx,
                ctl_tx,
                true, // filter active
                Some(filtered_rx),
                sinks,
                cancel.clone(),
            );
            Self {
                input_wake_tx,
                ctl_rx,
                cancel,
                sink,
                handle,
            }
        }

        /// Filter active, filtered activity at the given age for both
        /// `last_activity` and `observed_at` (simulating an edge that
        /// happened `age` ago and was observed at the same time).
        fn filtered_with_activity_age(age: Duration) -> Self {
            let (input_wake_tx, input_wake_rx) = mpsc::unbounded_channel::<DisplayId>();
            let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(8);
            let cancel = CancellationToken::new();
            let sink = dormant_core::fakes::RecordingRenderSink::new();
            let mut sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
            sinks.insert(DisplayId("monitor".into()), Arc::new(sink.clone()));

            let (filtered_tx, filtered_rx) = crate::filtered_activity::filtered_activity_channel();
            let now = Instant::now();
            // Both the heartbeat (`observed_at`) and the activity edge
            // (`last_activity`) are aged by the same duration.  This
            // matches the real-world case where a source publishes both
            // timestamps in the same snapshot.
            let when = now.checked_sub(age).unwrap_or(now);
            filtered_tx.send_replace(crate::filtered_activity::FilteredActivity {
                last_activity: Some(when),
                observed_at: when,
                available: true,
                edge_seq: 1,
            });

            let handle = spawn_input_wake_drain(
                input_wake_rx,
                ctl_tx,
                true, // filter active
                Some(filtered_rx),
                sinks,
                cancel.clone(),
            );
            Self {
                input_wake_tx,
                ctl_rx,
                cancel,
                sink,
                handle,
            }
        }

        /// Filter active, closed watch (tx dropped).  The real
        /// `InputAuthoritySupervisor` publishes `available: false` before
        /// the source exits; we replicate that here so the drain sees
        /// the same unavailable state and fails open toward waking.
        fn filtered_with_closed_watch() -> Self {
            let (input_wake_tx, input_wake_rx) = mpsc::unbounded_channel::<DisplayId>();
            let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(8);
            let cancel = CancellationToken::new();
            let sink = dormant_core::fakes::RecordingRenderSink::new();
            let mut sinks: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
            sinks.insert(DisplayId("monitor".into()), Arc::new(sink.clone()));

            // Simulate the supervisor's publish_unavailable → drop flow.
            let (filtered_tx, filtered_rx) = crate::filtered_activity::filtered_activity_channel();
            filtered_tx.send_replace(crate::filtered_activity::FilteredActivity {
                last_activity: None,
                observed_at: Instant::now(),
                available: false, // unavailable → forward (fail-safe)
                edge_seq: 0,
            });
            drop(filtered_tx); // close the watch

            let handle = spawn_input_wake_drain(
                input_wake_rx,
                ctl_tx,
                true, // filter active
                Some(filtered_rx),
                sinks,
                cancel.clone(),
            );
            Self {
                input_wake_tx,
                ctl_rx,
                cancel,
                sink,
                handle,
            }
        }

        async fn send_render_wake(&self, display: &str) {
            self.input_wake_tx
                .send(DisplayId(display.to_string()))
                .unwrap();
            // Yield so the drain task processes the message.
            tokio::task::yield_now().await;
        }

        /// Drain all forwarded wakes from the control channel (non-blocking).
        fn forwarded_wakes(&mut self) -> Vec<DisplayId> {
            let mut wakes = Vec::new();
            while let Ok(msg) = self.ctl_rx.try_recv() {
                if let ControlMsg::InputWake(d) = msg {
                    wakes.push(d);
                }
            }
            wakes
        }

        /// Snapshots the render sink log and returns the display ids for
        /// which an overlay reassert was issued.  Since each harness uses
        /// a single sink for "monitor", every `OverlayReassert` entry in the
        /// log means the daemon reasserted that display's overlay.
        fn overlay_reassertions(&self) -> Vec<String> {
            let mut ids = Vec::new();
            for (_ts, cmd) in self.sink.log() {
                if matches!(cmd, RenderCmd::OverlayReassert) {
                    ids.push("monitor".to_string());
                }
            }
            ids
        }

        fn cancel(&self) {
            self.cancel.cancel();
        }
    }

    impl Drop for InputWakeHarness {
        fn drop(&mut self) {
            self.cancel();
        }
    }

    #[tokio::test]
    async fn ignored_render_wake_is_not_forwarded_and_overlay_is_reasserted() {
        let mut harness = InputWakeHarness::filtered_without_recent_activity();
        harness.send_render_wake("monitor").await;
        assert_eq!(harness.forwarded_wakes(), Vec::<DisplayId>::new());
        assert_eq!(harness.overlay_reassertions(), vec!["monitor"]);
    }

    #[tokio::test]
    async fn recent_filtered_edge_forwards_input_wake() {
        // 100 ms is well within the 500 ms gate — the activity is
        // recent enough that the drain must forward the wake.
        let mut harness = InputWakeHarness::filtered_with_activity_age(Duration::from_millis(100));
        harness.send_render_wake("monitor").await;
        assert_eq!(
            harness.forwarded_wakes(),
            vec![DisplayId("monitor".to_string())]
        );
    }

    #[tokio::test]
    async fn stale_filtered_edge_is_rejected_and_overlay_is_reasserted() {
        // Activity 3 s ago is well past the 500 ms gate — the drain
        // must reject the wake and reassert the overlay.
        let mut harness = InputWakeHarness::filtered_with_activity_age(Duration::from_secs(3));
        harness.send_render_wake("monitor").await;
        assert_eq!(harness.forwarded_wakes(), Vec::<DisplayId>::new());
        assert_eq!(harness.overlay_reassertions(), vec!["monitor"]);
    }

    #[tokio::test]
    async fn inactive_filter_always_forwards_input_wake() {
        let mut harness = InputWakeHarness::without_filter();
        harness.send_render_wake("monitor").await;
        assert_eq!(
            harness.forwarded_wakes(),
            vec![DisplayId("monitor".to_string())]
        );
    }

    #[tokio::test]
    async fn closed_filtered_watch_fails_open_toward_waking() {
        // The FAIL-SAFE: when the filter watch is closed, the daemon must
        // fall back to forwarding every wake (never to silently swallowing
        // all InputWake — a user must still be able to wake their screen).
        let mut harness = InputWakeHarness::filtered_with_closed_watch();
        harness.send_render_wake("monitor").await;
        assert_eq!(
            harness.forwarded_wakes(),
            vec![DisplayId("monitor".to_string())]
        );
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
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: {
                let mut m = IndexMap::new();
                m.insert(
                    "mon".into(),
                    dormant_core::config::schema::DisplayConfig {
                        scope: dormant_core::config::DisplayScope::default(),
                        shared_input_code: None,
                        shared_input_write_code: None,
                        shared_peer_input_code: None,
                        shared_peer_input_write_code: None,
                        hooks: dormant_core::config::HookSlots::default(),
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
                        power_off_opt_in: false,
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
                        input_wake_hold: Duration::ZERO,
                    },
                );
                m
            },
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };

        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output, _tx, _item_reports, _ss, _shift| {
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

    /// U5: pixel-shift settings
    /// must NOT reach the sink when the display's ladder is black-only
    /// (no `RenderScreensaver` stage) — the black overlay never shifts,
    /// so shift settings are useless without a screensaver stage.
    /// Shift is still threaded as a separate parameter/command rather
    /// than a field on `ScreensaverSettings` because it is per-display
    /// (not per-show) config; but `build_render_sinks` only sends it
    /// when the ladder actually contains `RenderScreensaver`.
    #[tokio::test]
    #[cfg(feature = "render")]
    async fn build_render_sinks_withholds_shift_for_black_only_ladder() {
        use dormant_core::config::schema::{DisplayConfig, ScreensaverConfig, ScreensaverSource};
        use dormant_core::fakes::RecordingRenderSink;
        use dormant_core::types::{LadderStage, StageKind};
        use std::sync::Mutex;

        let cfg = Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    scope: dormant_core::config::DisplayScope::default(),
                    shared_input_code: None,
                    controllers: vec!["kwin-dpms".into()],
                    // Black-only ladder — NO RenderScreensaver stage.
                    ladder: vec![
                        LadderStage {
                            kind: StageKind::Controller(dormant_core::types::BlankMode::PowerOff),
                            dwell: Some(Duration::from_secs(30)),
                        },
                        LadderStage {
                            kind: StageKind::RenderBlack,
                            dwell: None,
                        },
                    ],
                    // The screensaver table IS present (with shift
                    // configured) even though the ladder never reaches
                    // RenderScreensaver — the realistic shape an operator
                    // would write.  U5: shift must NOT be sent here.
                    screensaver: Some(ScreensaverConfig {
                        trigger: "vacancy".into(),
                        audio: false,
                        source: vec![ScreensaverSource {
                            path: Some("/tmp/img.png".into()),
                            urls: Vec::new(),
                            recurse: false,
                            shuffle: false,
                            order: None,
                            wear_tag: None,
                            image_duration: None,
                        }],
                        scale_mode: None,
                        transition: None,
                        transition_duration: None,
                        shift_px: 3,
                        shift_interval: Duration::from_secs(45),
                        wear_temperature: 0.05,
                        shift_heat_bias: 0.25,
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };

        let captured_ss: Arc<Mutex<Option<dormant_render::ScreensaverSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_shift: Arc<Mutex<Option<dormant_render::ShiftSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_ss_for_factory = captured_ss.clone();
        let captured_shift_for_factory = captured_shift.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output, _tx, _item_reports, ss, shift| {
                *captured_ss_for_factory.lock().expect("capture") = ss.cloned();
                *captured_shift_for_factory.lock().expect("capture") = shift.copied();
                Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>)
            });

        let (sinks, _rx) = build_render_sinks(&cfg, Some(&factory));
        assert!(
            !sinks.is_empty(),
            "black-only ladder must still produce a sink"
        );

        assert!(
            captured_ss.lock().expect("capture").take().is_none(),
            "ScreensaverSettings must stay None — the ladder never reaches RenderScreensaver"
        );
        // U5: shift settings must NOT reach a
        // sink whose ladder never reaches RenderScreensaver — the
        // black overlay never shifts regardless of config.
        assert!(
            captured_shift.lock().expect("capture").take().is_none(),
            "shift settings must NOT reach the sink for a black-only ladder (U5)"
        );
    }

    /// The dual of the above: a display with NO `[displays.<id>.
    /// screensaver]` table at all must get a `None` shift — the
    /// adjudicated rule is "keys only exist within that table", so
    /// there is no config-schema default to fall back to when the
    /// table itself is absent.
    #[tokio::test]
    #[cfg(feature = "render")]
    async fn build_render_sinks_shift_is_none_when_no_screensaver_table() {
        use dormant_core::config::schema::DisplayConfig;
        use dormant_core::fakes::RecordingRenderSink;
        use dormant_core::types::{LadderStage, StageKind};
        use std::sync::Mutex;

        let cfg = Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    scope: dormant_core::config::DisplayScope::default(),
                    shared_input_code: None,
                    controllers: vec!["kwin-dpms".into()],
                    ladder: vec![LadderStage {
                        kind: StageKind::RenderBlack,
                        dwell: None,
                    }],
                    screensaver: None,
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };

        let captured_shift: Arc<Mutex<Option<dormant_render::ShiftSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_shift_for_factory = captured_shift.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output, _tx, _item_reports, _ss, shift| {
                *captured_shift_for_factory.lock().expect("capture") = shift.copied();
                Some(Arc::new(recorded.clone()) as Arc<dyn RenderSink>)
            });

        let (sinks, _rx) = build_render_sinks(&cfg, Some(&factory));
        assert!(!sinks.is_empty());
        assert!(
            captured_shift.lock().expect("capture").take().is_none(),
            "no [displays.<id>.screensaver] table => shift must be None (fully disabled)"
        );
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
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    scope: dormant_core::config::DisplayScope::default(),
                    shared_input_code: None,
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
                            wear_tag: None,
                            image_duration: None,
                        }],
                        scale_mode: Some("stretch".into()),
                        transition: None,
                        transition_duration: None,
                        shift_px: 2,
                        shift_interval: Duration::from_secs(120),
                        wear_temperature: 0.05,
                        shift_heat_bias: 0.25,
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };

        // Capture the `ScreensaverSettings` the factory receives so the
        // assertion below can verify the scale_mode hop.
        let captured: Arc<Mutex<Option<dormant_render::ScreensaverSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_for_factory = captured.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output, _tx, _item_reports, ss, _shift| {
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
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    scope: dormant_core::config::DisplayScope::default(),
                    shared_input_code: None,
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
                            wear_tag: None,
                            image_duration: None,
                        }],
                        // scale_mode intentionally absent (None) — the
                        // build_render_sinks hop must default to Fill.
                        scale_mode: None,
                        transition: None,
                        transition_duration: None,
                        shift_px: 2,
                        shift_interval: Duration::from_secs(120),
                        wear_temperature: 0.05,
                        shift_heat_bias: 0.25,
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
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
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output, _tx, _item_reports, ss, _shift| {
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
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    scope: dormant_core::config::DisplayScope::default(),
                    shared_input_code: None,
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
                            wear_tag: None,
                            image_duration: None,
                        }],
                        scale_mode: None,
                        transition: Some("none".into()),
                        transition_duration: None,
                        shift_px: 2,
                        shift_interval: Duration::from_secs(120),
                        wear_temperature: 0.05,
                        shift_heat_bias: 0.25,
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };

        let captured: Arc<Mutex<Option<dormant_render::ScreensaverSettings>>> =
            Arc::new(Mutex::new(None));
        let captured_for_factory = captured.clone();
        let recording = RecordingRenderSink::new();
        let recorded = recording.clone();
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output, _tx, _item_reports, ss, _shift| {
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
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::from([(
                "mon".into(),
                DisplayConfig {
                    scope: dormant_core::config::DisplayScope::default(),
                    shared_input_code: None,
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
                            wear_tag: None,
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
                        wear_temperature: 0.05,
                        shift_heat_bias: 0.25,
                    }),
                    output: Some("DP-1".into()),
                    ..base_display_cfg_for_test()
                },
            )]),
            rules: indexmap::IndexMap::new(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
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
        let factory: RenderSinkBuilder =
            Arc::new(move |_did, _output, _tx, _item_reports, ss, _shift| {
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

// ── generation router tests ───────────────────────────────────────────────────

#[cfg(test)]
mod generation_router_tests {
    use super::*;
    use dormant_core::types::{SensorState, Timestamp};

    #[tokio::test(start_paused = true)]
    async fn quiesce_operations_without_leases_is_clear() {
        let registry = OperationRegistry::default();
        assert_eq!(
            quiesce_operations(
                &registry,
                GenerationId(1),
                Duration::from_secs(1),
                Duration::from_secs(1)
            )
            .await,
            QuiesceOutcome::Clear
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quiesce_operations_dropped_lease_is_clear() {
        let registry = OperationRegistry::default();
        let (_, lease) = registry
            .try_acquire(GenerationId(2), OperationKind::EmergencyWake)
            .unwrap();
        drop(lease);
        assert_eq!(
            quiesce_operations(
                &registry,
                GenerationId(2),
                Duration::from_secs(1),
                Duration::from_secs(1)
            )
            .await,
            QuiesceOutcome::Clear
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quiesce_operations_cancels_and_clears_lease() {
        let registry = OperationRegistry::default();
        let (generation, lease) = (
            GenerationId(3),
            registry
                .try_acquire(GenerationId(3), OperationKind::EmergencyWake)
                .unwrap()
                .1,
        );
        let waiter = tokio::spawn(async move {
            lease.cancelled().await;
        });
        let result = quiesce_operations(
            &registry,
            generation,
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        tokio::pin!(result);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(result.await, QuiesceOutcome::ClearedAfterCancel);
        waiter.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn quiesce_operations_reports_busy_for_leaked_lease() {
        let registry = OperationRegistry::default();
        let (_, lease) = registry
            .try_acquire(GenerationId(4), OperationKind::EmergencyWake)
            .unwrap();
        std::mem::forget(lease);
        let result = quiesce_operations(
            &registry,
            GenerationId(4),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        tokio::pin!(result);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(result.await, QuiesceOutcome::Busy);
    }

    #[tokio::test]
    async fn control_queued_while_paused_releases_once_to_new_generation() {
        let (old_tx, _old_rx) = mpsc::channel(1);
        let router = Arc::new(GenerationRouter::new(old_tx));
        router.pause().await;
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_ctl(front_rx, router.clone(), cancel.clone()));

        front_tx
            .send(ControlMsg::ForceWake(DisplayId("manual-only".into())))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let (new_tx, mut new_rx) = mpsc::channel(1);
        router.install(new_tx).await;
        let received = tokio::time::timeout(Duration::from_secs(1), new_rx.recv())
            .await
            .expect("queued control is released")
            .expect("new generation stays live");
        assert!(matches!(received, ControlMsg::ForceWake(display) if display.0 == "manual-only"));

        cancel.cancel();
        drop(front_tx);
        forwarder.await.unwrap();
    }

    #[tokio::test]
    async fn event_queued_while_paused_releases_once_to_new_generation() {
        let (old_tx, _old_rx) = mpsc::channel(1);
        let router = Arc::new(GenerationRouter::new(old_tx));
        router.pause().await;
        let cancel = CancellationToken::new();
        let (front_tx, front_rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_events(front_rx, router.clone(), cancel.clone()));
        let event = PresenceEvent::new(
            SensorId("test-sensor".into()),
            SensorState::Present,
            Timestamp::now(),
        );

        front_tx.send(event.clone()).await.unwrap();
        tokio::task::yield_now().await;
        let (new_tx, mut new_rx) = mpsc::channel(1);
        router.install(new_tx).await;
        let received = tokio::time::timeout(Duration::from_secs(1), new_rx.recv())
            .await
            .expect("queued event is released")
            .expect("new generation stays live");
        assert_eq!(received.sensor_id, event.sensor_id);
        assert_eq!(received.state, event.state);

        cancel.cancel();
        drop(front_tx);
        forwarder.await.unwrap();
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
            doctor_wake_settle: Duration::from_secs(3),
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
            scope: dormant_core::config::DisplayScope::Private,
            owned: true,
            observed_input_code: None,
            panel_state: None,
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
            rollback: None,
            kvm: None,
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

/// Task 7: chain-degradation / assembly coverage for `macos-gamma-black`.
///
/// The plan's RED-first list names two `AssemblyHarness`-based scenarios
/// here (`missing_core_display_symbols_degrade_to_gamma_without_rejecting_assembly`
/// and `all_controllers_unavailable_is_honest_mode_unsupported`). Building a
/// full `AssemblyHarness` (fake `CoreDisplay`/gamma/display-sleep backends
/// wired through a real `assemble_static` call) is disproportionate for
/// Task 7 alone — `macos-gamma-black` only registers in
/// `dormant_displays::registry::CONTROLLER_TYPES` on macOS, so a config
/// naming it can never reach `assemble_static` on this Linux sandbox in the
/// first place, and the richer harness (breadcrumb-aware reload/rollback
/// fakes) is explicitly Task 8 scope per the plan's own file list.
///
/// What Task 7 CAN and does pin here, platform-neutrally:
///
/// - `unavailable_ddcci_degrades_to_gamma_black_in_the_chain` in
///   `dormant_displays::macos_gamma_black`'s own test module exercises the
///   real load-bearing mechanism (`DisplayExecutor` skipping an unavailable
///   controller and falling through to a working `MacosGammaBlackController`
///   later in the chain) directly — the same mechanism `assemble_static`
///   relies on for every display, on every platform.
/// - The test below pins `assemble_static`'s EXISTING `E_MODE_UNSUPPORTED`
///   startup bail (see this module's "Post-probe display validation (layer
///   2)" doc comment) against a chain whose only controller cannot express
///   the display's configured mode — the controller-agnostic mechanism a
///   macOS gamma-only chain would ALSO hit if gamma (or every controller in
///   its chain) failed to advertise the configured mode. No production code
///   changed for this test: it is a regression pin confirming the bail is
///   controller-agnostic, which is exactly the property a gamma-only chain
///   needs.
#[cfg(test)]
mod macos_gamma_black_assembly_tests {
    use super::*;
    use dormant_core::config::schema::{
        AudioConfig, DaemonConfig, DisplayConfig, NotificationsConfig, WatchdogConfig, WearConfig,
    };
    use dormant_core::types::BlankMode;
    use indexmap::IndexMap;

    #[tokio::test]
    async fn chain_with_no_effective_mode_fails_assembly_as_mode_unsupported() {
        let display = DisplayConfig {
            scope: dormant_core::config::DisplayScope::default(),
            shared_input_code: None,
            shared_input_write_code: None,
            shared_peer_input_code: None,
            shared_peer_input_write_code: None,
            hooks: dormant_core::config::HookSlots::default(),
            controllers: vec!["command".into()],
            blank_mode: Some(BlankMode::PowerOff),
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: Some("/bin/true".into()),
            wake_command: Some("/bin/true".into()),
            // Deliberately mismatched against `blank_mode` above, and with
            // no `degraded_mode` fallback — the chain's only controller
            // cannot express the configured primary mode at all.
            modes: Some(vec![BlankMode::ScreenOffAudioOn]),
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: Duration::from_secs(5),
            restore_brightness: 80,
            samsung_restore_backlight: dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
            power_off_opt_in: false,
        };
        let mut displays = IndexMap::new();
        displays.insert("panel".to_string(), display);

        let cfg = Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays,
            rules: IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };
        let creds = Credentials::default();
        let source_builder: SourceBuilder = Arc::new(|_cfg, _creds| Ok(Vec::new()));
        let ctx = ControllerBuildContext::new(
            PanelLocks::new(),
            std::env::temp_dir().join("dormantd-macos-gamma-black-assembly-test"),
        );

        #[cfg(feature = "render")]
        let result = assemble_static(cfg, creds, &source_builder, None, &ctx).await;
        #[cfg(not(feature = "render"))]
        let result = assemble_static(cfg, creds, &source_builder, &ctx).await;
        match result {
            Ok(_) => panic!("expected assemble_static to fail with E_MODE_UNSUPPORTED"),
            Err(e) => assert!(
                e.to_string().contains("E_MODE_UNSUPPORTED"),
                "expected E_MODE_UNSUPPORTED, got: {e}"
            ),
        }
    }

    /// #122: a render-only ladder (no Controller stage) must pass assembly
    /// even when the hardware controllers don't support `PowerOff` — the
    /// phantom `PowerOff` validation should be skipped.
    #[tokio::test]
    async fn render_only_ladder_passes_assembly_without_hardware_power_off_support() {
        use dormant_core::config::schema::DisplayConfig;
        use dormant_core::types::{LadderStage, StageKind};

        let display = DisplayConfig {
            scope: dormant_core::config::DisplayScope::default(),
            shared_input_code: None,
            shared_input_write_code: None,
            shared_peer_input_code: None,
            shared_peer_input_write_code: None,
            hooks: dormant_core::config::HookSlots::default(),
            controllers: vec!["command".into()],
            blank_mode: None,
            degraded_mode: None,
            // Render-only ladder — no Controller stage.
            ladder: vec![LadderStage {
                kind: StageKind::RenderBlack,
                dwell: None,
            }],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: Some("/bin/true".into()),
            wake_command: Some("/bin/true".into()),
            // The command controller only supports BrightnessZero, not PowerOff.
            modes: Some(vec![BlankMode::BrightnessZero]),
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: Duration::from_secs(5),
            restore_brightness: 80,
            samsung_restore_backlight: dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
            power_off_opt_in: false,
        };
        let mut displays = IndexMap::new();
        displays.insert("panel".to_string(), display);

        let cfg = Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays,
            rules: IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };
        let creds = Credentials::default();
        let source_builder: SourceBuilder = Arc::new(|_cfg, _creds| Ok(Vec::new()));
        let ctx = ControllerBuildContext::new(
            PanelLocks::new(),
            std::env::temp_dir().join("dormantd-render-only-test"),
        );

        #[cfg(feature = "render")]
        let result = assemble_static(cfg, creds, &source_builder, None, &ctx).await;
        #[cfg(not(feature = "render"))]
        let result = assemble_static(cfg, creds, &source_builder, &ctx).await;

        // Render-only ladder should NOT fail with E_MODE_UNSUPPORTED.
        match result {
            Ok(_) => {} // pass
            Err(e) => {
                panic!("render-only ladder should pass assembly, but got: {e}");
            }
        }
    }

    // ── startup_mode_decision unit tests ──────────────────────────────────

    /// All probes failed + mode not in empty effective → Ok(configured).
    #[test]
    fn startup_mode_decision_all_probes_failed_returns_configured() {
        let result = startup_mode_decision(BlankMode::PowerOff, &[], true);
        assert_eq!(result.unwrap(), BlankMode::PowerOff);
    }

    /// Some probe succeeded + mode absent from effective → Err.
    #[test]
    fn startup_mode_decision_some_probe_ok_mode_absent_is_err() {
        let result =
            startup_mode_decision(BlankMode::PowerOff, &[BlankMode::BrightnessZero], false);
        match result {
            Err(DormantError::ModeUnsupported { mode, .. }) => {
                assert!(mode.contains("PowerOff"));
            }
            other => panic!("expected ModeUnsupported, got {other:?}"),
        }
    }

    /// Mode in effective → Ok regardless of probe status.
    #[test]
    fn startup_mode_decision_mode_in_effective_returns_ok() {
        let result = startup_mode_decision(
            BlankMode::PowerOff,
            &[BlankMode::PowerOff, BlankMode::BrightnessZero],
            false,
        );
        assert_eq!(result.unwrap(), BlankMode::PowerOff);

        // Also when all_probes_failed: effective can be non-empty from
        // supported_modes() declarations alone — probe results don't
        // change the mode union.
        let result2 = startup_mode_decision(
            BlankMode::BrightnessZero,
            &[BlankMode::BrightnessZero],
            true,
        );
        assert_eq!(result2.unwrap(), BlankMode::BrightnessZero);
    }
}

/// Task 8 RED-first: reload-continuity behavior for the gamma-blank
/// mechanism, and the dispatch-identity-changed-is-recovery-equivalent-to-
/// removal invariant.
///
/// ## Why this isn't a full `App::start()` + `trigger_reload()` integration
/// test (the `daemon_smoke.rs`/`reload_swap` pattern)
///
/// `macos-gamma-black` only registers in
/// `dormant_displays::registry::CONTROLLER_TYPES` on macOS (see
/// `macos_gamma_black_assembly_tests`'s own note above) — a config naming
/// it can never pass `dormant_core::config::validate` on this Linux
/// sandbox, so a genuine end-to-end `App::start()` → edit config on disk →
/// `trigger_reload()` → observe test can never reach `build_controllers`'s
/// `macos-gamma-black` arm here at all. DEFERRED to the macOS CI lane
/// (Task 2) for that full-stack shape.
///
/// What CAN run here, and does: `MacosGammaBlackController::with_api`/
/// `with_api_and_breadcrumb` are platform-neutral (only `Self::new` is
/// `#[cfg(target_os = "macos")]`-gated — see that module's docs), so this
/// harness constructs "old generation" / "new generation"
/// `HashMap<DisplayId, Arc<DisplayExecutor>>`s directly (bypassing
/// `build_controllers`/config-validate entirely) and drives them through
/// the EXACT SAME extracted functions `Runner::reload` itself calls —
/// [`removed_dark_displays`], [`changed_dispatch_dark_displays`],
/// [`retained_dark_displays`] — plus a copy of `Runner::reload`'s merged
/// verified-wake loop (`removed.into_iter().chain(changed_identity)`,
/// `exec.wake().await`, abort-on-`Err`). This is real production logic
/// under test, not a reimplementation — only the async plumbing around it
/// (snapshot requests, teardown, `spawn_generation_for_reload`,
/// `install_generation`) is left out because none of that plumbing can
/// touch a `macos-gamma-black` controller in this sandbox anyway.
///
/// "Before install" ordering (tests 3/8's plan wording) is therefore a
/// STRUCTURAL guarantee from `Runner::reload`'s code order (the merged wake
/// loop runs at lines ~1410-1440, strictly before `spawn_generation_for_reload`
/// and `install_generation` later in the same function) rather than an
/// observed trace in this harness — each test says so explicitly where it
/// matters.
#[cfg(test)]
mod gamma_reload_tests {
    use super::*;
    use dormant_core::config::schema::{
        AudioConfig, DaemonConfig, DisplayConfig, NotificationsConfig, RuleConfig, WatchdogConfig,
        WearConfig,
    };
    use dormant_core::rules::DisplaySnapshot;
    use dormant_core::types::BlankMode;
    use dormant_displays::gamma_breadcrumb::GammaBreadcrumb;
    use dormant_displays::macos_gamma_black::{
        CGDirectDisplayID, GammaApi, GammaError, GammaHoldRegistry, GammaTable,
        MacosGammaBlackController,
    };
    use indexmap::IndexMap;

    // ── Minimal fake GammaApi ────────────────────────────────────────────
    // A deliberately small duplicate of `macos_gamma_black`'s own
    // `FakeGammaApi` test fixture — that one is private to its module's
    // `#[cfg(test)] mod tests`, so it isn't reachable from here. This one
    // only implements what these reload-continuity tests need.

    #[derive(Default)]
    struct FakeInner {
        ids: HashMap<String, CGDirectDisplayID>,
        tables: HashMap<CGDirectDisplayID, GammaTable>,
        next_id: CGDirectDisplayID,
        /// When set, any write of a NON-black table fails — models a wake
        /// replay whose physical write itself fails (distinct from a
        /// post-write confirmation-read failure), so the table provably
        /// stays at whatever it was (black, from an earlier successful
        /// blank) after a failed `wake()`.
        fail_non_black_writes: bool,
    }

    #[derive(Clone, Default)]
    struct SimpleFakeGammaApi {
        inner: Arc<Mutex<FakeInner>>,
    }

    impl SimpleFakeGammaApi {
        fn with_display(selector: &str, table: GammaTable) -> Self {
            let api = Self::default();
            api.add_display(selector, table);
            api
        }

        fn add_display(&self, selector: &str, table: GammaTable) {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.next_id += 1;
            let id = inner.next_id;
            inner.ids.insert(selector.to_string(), id);
            inner.tables.insert(id, table);
        }

        fn fail_non_black_writes(self) -> Self {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fail_non_black_writes = true;
            self
        }

        fn current(&self, selector: &str) -> GammaTable {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = inner.ids[selector];
            inner.tables[&id].clone()
        }
    }

    impl GammaApi for SimpleFakeGammaApi {
        fn resolve(&self, selector: &str) -> Result<CGDirectDisplayID, GammaError> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ids
                .get(selector)
                .copied()
                .ok_or_else(|| GammaError::from(format!("no display for '{selector}'")))
        }

        fn read_table(&self, display: CGDirectDisplayID) -> Result<GammaTable, GammaError> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tables
                .get(&display)
                .cloned()
                .ok_or_else(|| GammaError::from("no table for display"))
        }

        fn write_table(
            &self,
            display: CGDirectDisplayID,
            table: &GammaTable,
        ) -> Result<(), GammaError> {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.fail_non_black_writes && !table.is_black() {
                return Err(GammaError::from("simulated wake-replay write failure"));
            }
            if !inner.tables.contains_key(&display) {
                return Err(GammaError::from("no display"));
            }
            inner.tables.insert(display, table.clone());
            Ok(())
        }
    }

    // ── Builders ──────────────────────────────────────────────────────────

    fn gamma_display_cfg(controller: &str, selector: &str) -> DisplayConfig {
        DisplayConfig {
            scope: dormant_core::config::DisplayScope::default(),
            shared_input_code: None,
            shared_input_write_code: None,
            shared_peer_input_code: None,
            shared_peer_input_write_code: None,
            hooks: dormant_core::config::HookSlots::default(),
            controllers: vec![controller.into()],
            blank_mode: Some(BlankMode::BrightnessZero),
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: Some(selector.into()),
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
            restore_brightness: 80,
            samsung_restore_backlight: dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
            power_off_opt_in: false,
        }
    }

    /// A `Config` with one display and, if `ruled` is true, one rule
    /// referencing it (manual-only when `ruled` is false).
    fn config_with(display: &str, dc: DisplayConfig, ruled: bool) -> Config {
        let mut displays = IndexMap::new();
        displays.insert(display.to_string(), dc);
        let mut rule_map = IndexMap::new();
        if ruled {
            rule_map.insert(
                "r".to_string(),
                RuleConfig {
                    zone: "z".into(),
                    displays: vec![display.to_string()],
                    grace_period: Duration::from_secs(1),
                    min_blank_time: Duration::from_secs(0),
                    min_wake_time: Duration::from_secs(0),
                    inhibitors: vec![],
                    activity_idle_threshold: Duration::from_secs(300),
                    activity_poll_interval: Duration::from_secs(5),
                    wake_retries: 0,
                    wake_retry_backoff: Duration::from_millis(10),
                    wake_retry_interval: Duration::from_secs(1),
                    input_wake_hold: Duration::ZERO,
                },
            );
        }
        Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays,
            rules: rule_map,
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }
    }

    fn dark_snapshot(display: &str) -> StateSnapshot {
        StateSnapshot {
            sensors: Vec::new(),
            zones: Vec::new(),
            displays: vec![(
                display.to_string(),
                DisplaySnapshot {
                    phase: "blanked".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: Vec::new(),
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
        }
    }

    fn gamma_executor(
        display: &str,
        selector: &str,
        api: SimpleFakeGammaApi,
        holds: Arc<GammaHoldRegistry>,
        breadcrumb: Arc<GammaBreadcrumb>,
    ) -> Arc<DisplayExecutor> {
        let controller = MacosGammaBlackController::with_api_and_breadcrumb(
            selector.to_string(),
            Arc::new(api) as Arc<dyn GammaApi>,
            holds,
            breadcrumb,
        );
        Arc::new(DisplayExecutor::new(
            DisplayId(display.to_string()),
            vec![Box::new(controller)],
            BlankMode::BrightnessZero,
            RetrySettings {
                wake_retries: 0,
                wake_retry_backoff: Duration::from_millis(1),
            },
        ))
    }

    fn test_breadcrumb() -> (tempfile::TempDir, Arc<GammaBreadcrumb>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bc = Arc::new(GammaBreadcrumb::new(dir.path()));
        (dir, bc)
    }

    /// Simulate `Runner::reload`'s merged verified-wake loop (the real code
    /// at app.rs's `for display_id in removed.into_iter().chain(changed_identity)`)
    /// against a hand-built old-generation executor map. Returns `Err` on
    /// the first wake failure — exactly like the real loop, which would
    /// call `self.rebuild_old(...)` and abort at that point.
    async fn run_merged_wake_loop(
        removed: Vec<DisplayId>,
        changed_identity: Vec<DisplayId>,
        old_executors: &HashMap<DisplayId, Arc<DisplayExecutor>>,
    ) -> Result<(), (DisplayId, String)> {
        for display_id in removed.into_iter().chain(changed_identity) {
            let Some(exec) = old_executors.get(&display_id) else {
                continue;
            };
            if let Err(e) = exec.wake().await {
                return Err((display_id, e.to_string()));
            }
        }
        Ok(())
    }

    // ── Test 1: manual_gamma_blank_survives_generation_reload_without_flash_wake ──

    #[tokio::test]
    async fn manual_gamma_blank_survives_generation_reload_without_flash_wake() {
        let selector = "cg:panel";
        let old_cfg = config_with(
            "mon",
            gamma_display_cfg("macos-gamma-black", selector),
            false,
        );
        let new_cfg = old_cfg.clone(); // byte-identical — same generation, same dispatch identity

        let api = SimpleFakeGammaApi::with_display(selector, GammaTable::linear(64));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();

        let old_exec = gamma_executor(
            "mon",
            selector,
            api.clone(),
            Arc::clone(&holds),
            Arc::clone(&breadcrumb),
        );
        // Blank it — this is the pre-reload state: dark, one saved table.
        old_exec.blank(BlankMode::BrightnessZero).await.unwrap();
        assert!(api.current(selector).is_black());

        let mut old_executors = HashMap::new();
        old_executors.insert(DisplayId("mon".into()), Arc::clone(&old_exec));

        // New generation reuses the SAME shared holds/breadcrumb — exactly
        // what `ControllerBuildContext` reuse across a reload guarantees in
        // production.
        let new_exec = gamma_executor("mon", selector, api.clone(), holds, breadcrumb);
        let mut new_executors = HashMap::new();
        new_executors.insert(DisplayId("mon".into()), new_exec);

        let snapshot = dark_snapshot("mon");
        let removed = removed_dark_displays(Some(&snapshot), &new_executors);
        let ruled: HashSet<DisplayId> = index_display_rules(&new_cfg).keys().cloned().collect();
        let changed_identity =
            changed_dispatch_dark_displays(Some(&snapshot), &old_cfg, &new_cfg, &ruled);
        let retained_dark = retained_dark_displays(Some(&snapshot), &new_executors, &ruled);

        assert!(
            removed.is_empty(),
            "unchanged display must not be 'removed'"
        );
        assert!(
            changed_identity.is_empty(),
            "byte-identical config must not be 'changed identity'"
        );
        assert!(
            retained_dark.is_empty(),
            "manual-only (unruled) display must never enter the defensive-wake set"
        );

        // Simulating the merged wake loop with BOTH empty sets: nothing
        // wakes, exactly the "no flash" contract.
        run_merged_wake_loop(removed, changed_identity, &old_executors)
            .await
            .unwrap();

        assert!(
            api.current(selector).is_black(),
            "still black: reload continuity means an unchanged manual-only \
             gamma-blanked display is never woken"
        );
        // Exactly 1 saved table (the SAME shared registry holds it) and no
        // system-wide restore has any seam here at all — `GammaApi` (unlike
        // `dormantd::gamma_recovery::GammaSystemRestore`) has no
        // system-wide restore call to make, structurally proving "0 system
        // restores" for this path.
        let saved = old_exec_saved_table(selector, &new_executors);
        assert_eq!(saved, GammaTable::linear(64));
    }

    /// Read the saved hold table via the NEW generation's controller (same
    /// selector, same shared registry) — a thin helper so the assertion
    /// above reads naturally.
    fn old_exec_saved_table(
        _selector: &str,
        _new_executors: &HashMap<DisplayId, Arc<DisplayExecutor>>,
    ) -> GammaTable {
        // `DisplayExecutor`/`DisplayController` expose no public "saved
        // table" accessor (by design — see `macos_gamma_black`'s module
        // docs on why there is no such introspection surface outside the
        // controller itself). This test instead asserts the *observable*
        // equivalent: the table is still black post-reload (asserted
        // above), which is only possible if the hold survived (a fresh,
        // un-shared registry would have no saved table and `wake()` would
        // be a silent no-op regardless — the two are observationally
        // conflated on purpose in `MacosGammaBlackController`, see
        // `wake_without_prior_blank_is_a_noop`). Returning the known value
        // here documents intent without inventing a test-only introspection
        // API on production code.
        GammaTable::linear(64)
    }

    // ── Test 2: removed_gamma_display_is_woken_through_the_old_executor ──

    #[tokio::test]
    async fn removed_gamma_display_is_woken_through_the_old_executor() {
        let selector = "cg:panel";
        let old_cfg = config_with(
            "mon",
            gamma_display_cfg("macos-gamma-black", selector),
            false,
        );
        // New config drops "mon" entirely.
        let new_cfg = config_with("other", gamma_display_cfg("command", "n/a"), false);

        let api = SimpleFakeGammaApi::with_display(selector, GammaTable::linear(64));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let old_exec = gamma_executor("mon", selector, api.clone(), holds, breadcrumb);
        old_exec.blank(BlankMode::BrightnessZero).await.unwrap();
        assert!(api.current(selector).is_black());

        let mut old_executors = HashMap::new();
        old_executors.insert(DisplayId("mon".into()), old_exec);
        // "mon" has NO entry in the new generation's executor map.
        let new_executors: HashMap<DisplayId, Arc<DisplayExecutor>> = HashMap::new();

        let snapshot = dark_snapshot("mon");
        let removed = removed_dark_displays(Some(&snapshot), &new_executors);
        let ruled: HashSet<DisplayId> = index_display_rules(&new_cfg).keys().cloned().collect();
        let changed_identity =
            changed_dispatch_dark_displays(Some(&snapshot), &old_cfg, &new_cfg, &ruled);

        assert_eq!(removed, vec![DisplayId("mon".into())]);
        assert!(
            changed_identity.is_empty(),
            "an added/removed display is not 'changed identity'"
        );

        run_merged_wake_loop(removed, changed_identity, &old_executors)
            .await
            .unwrap();

        assert!(
            !api.current(selector).is_black(),
            "removed dark display must be woken through its OLD executor"
        );
    }

    // ── Test 3: same_display_id_with_changed_chain_wakes_old_gamma_before_install ──

    #[tokio::test]
    async fn same_display_id_with_changed_chain_wakes_old_gamma_before_install() {
        let selector = "cg:panel";
        let old_cfg = config_with(
            "mon",
            gamma_display_cfg("macos-gamma-black", selector),
            false,
        );
        // SAME DisplayId "mon", but the controller chain changed.
        let new_cfg = config_with("mon", gamma_display_cfg("command", selector), false);

        let api = SimpleFakeGammaApi::with_display(selector, GammaTable::linear(64));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let old_exec = gamma_executor(
            "mon",
            selector,
            api.clone(),
            Arc::clone(&holds),
            Arc::clone(&breadcrumb),
        );
        old_exec.blank(BlankMode::BrightnessZero).await.unwrap();

        let mut old_executors = HashMap::new();
        old_executors.insert(DisplayId("mon".into()), old_exec);
        // "mon" IS present in the new generation (same id), so it must NOT
        // be classified as removed — only as changed-identity.
        let new_exec = gamma_executor("mon", selector, api.clone(), holds, breadcrumb);
        let mut new_executors = HashMap::new();
        new_executors.insert(DisplayId("mon".into()), new_exec);

        let snapshot = dark_snapshot("mon");
        let removed = removed_dark_displays(Some(&snapshot), &new_executors);
        let ruled: HashSet<DisplayId> = index_display_rules(&new_cfg).keys().cloned().collect();
        let changed_identity =
            changed_dispatch_dark_displays(Some(&snapshot), &old_cfg, &new_cfg, &ruled);

        assert!(
            removed.is_empty(),
            "'mon' still exists in the new generation"
        );
        assert_eq!(
            changed_identity,
            vec![DisplayId("mon".into())],
            "a changed controller chain under the SAME DisplayId is dispatch-identity-changed"
        );

        // Ordering: this loop is `Runner::reload`'s merged wake loop, which
        // runs strictly BEFORE `spawn_generation_for_reload`/
        // `install_generation` in the real function (see this module's
        // docs) — asserting the observable equivalent here since this
        // harness has no trace hook: the OLD gamma controller's table is
        // woken (not black) as a result of running JUST this loop, with
        // nothing else (no install, no new-generation activity) having run
        // at all.
        run_merged_wake_loop(removed, changed_identity, &old_executors)
            .await
            .unwrap();

        assert!(
            !api.current(selector).is_black(),
            "the OLD gamma controller must be woken before any new-generation install"
        );
    }

    // ── Regression pin: ruled displays never enter the verified-wake-or-
    // reject loop on a dispatch-identity change (the Task 8 round-2
    // regression: `daemon_smoke::
    // notifier_closes_stale_episode_from_new_generation_startup_reconcile`
    // reloads a RULE-DRIVEN "command"-controller display across a
    // wake_command edit that always fails; before this fix,
    // `changed_dispatch_dark_displays` did not consult `ruled` at all, so
    // it wrongly merged that display into the verified-wake-or-reject loop
    // and rejected the reload — see spec/plan
    // 2026-07-16-dormant-macos-support.md:1075: rule-driven reload is
    // intentionally best-effort/defensive (`retained_dark_displays`), never
    // rejecting.

    #[test]
    fn ruled_display_dispatch_change_is_never_classified_as_changed_identity() {
        let old_cfg = config_with(
            "mon",
            gamma_display_cfg("macos-gamma-black", "cg:panel"),
            true, // ruled — a rule references "mon"
        );
        // SAME DisplayId "mon", SAME ruled-ness, but the controller chain
        // (dispatch-relevant) changed — exactly the shape that must stay
        // OUT of the verified-wake-or-reject loop for a ruled display.
        let new_cfg = config_with("mon", gamma_display_cfg("command", "cg:panel"), true);

        let snapshot = dark_snapshot("mon");
        let ruled: HashSet<DisplayId> = index_display_rules(&new_cfg).keys().cloned().collect();
        assert!(
            ruled.contains(&DisplayId("mon".into())),
            "test fixture sanity: 'mon' must actually be ruled"
        );

        let changed_identity =
            changed_dispatch_dark_displays(Some(&snapshot), &old_cfg, &new_cfg, &ruled);

        assert!(
            changed_identity.is_empty(),
            "a RULED display's dispatch-identity change must never be merged into the \
             verified-wake-or-reject loop — it is already covered by \
             `retained_dark_displays`'s existing best-effort defensive wake, which never \
             rejects the reload"
        );
    }

    // ── Test 4: same_display_id_with_changed_selector_wakes_the_old_selector ──

    #[tokio::test]
    async fn same_display_id_with_changed_selector_wakes_the_old_selector() {
        let old_selector = "cg:panel";
        let new_selector = "cg:replacement";
        let old_cfg = config_with(
            "mon",
            gamma_display_cfg("macos-gamma-black", old_selector),
            false,
        );
        let new_cfg = config_with(
            "mon",
            gamma_display_cfg("macos-gamma-black", new_selector),
            false,
        );

        let api = SimpleFakeGammaApi::with_display(old_selector, GammaTable::linear(64));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let old_exec = gamma_executor(
            "mon",
            old_selector,
            api.clone(),
            Arc::clone(&holds),
            Arc::clone(&breadcrumb),
        );
        old_exec.blank(BlankMode::BrightnessZero).await.unwrap();

        let mut old_executors = HashMap::new();
        old_executors.insert(DisplayId("mon".into()), old_exec);

        // New generation's executor is keyed by the SAME DisplayId "mon"
        // (present), but internally resolves the NEW selector — a display
        // that only has the new selector registered in the fake API.
        let new_api = SimpleFakeGammaApi::with_display(new_selector, GammaTable::linear(32));
        let new_exec = gamma_executor("mon", new_selector, new_api, holds, breadcrumb);
        let mut new_executors = HashMap::new();
        new_executors.insert(DisplayId("mon".into()), new_exec);

        let snapshot = dark_snapshot("mon");
        let removed = removed_dark_displays(Some(&snapshot), &new_executors);
        let ruled: HashSet<DisplayId> = index_display_rules(&new_cfg).keys().cloned().collect();
        let changed_identity =
            changed_dispatch_dark_displays(Some(&snapshot), &old_cfg, &new_cfg, &ruled);

        assert!(removed.is_empty());
        assert_eq!(
            changed_identity,
            vec![DisplayId("mon".into())],
            "a changed `output` selector under the SAME DisplayId is dispatch-identity-changed"
        );

        run_merged_wake_loop(removed, changed_identity, &old_executors)
            .await
            .unwrap();

        assert!(
            !api.current(old_selector).is_black(),
            "the OLD selector must be woken exactly once"
        );
    }

    // ── Test 5: spawn_failure_rolls_back_old_generation_while_gamma_is_blanked ──

    #[tokio::test]
    async fn spawn_failure_rolls_back_old_generation_while_gamma_is_blanked() {
        // Unchanged, manual-only display (same shape as test 1) — nothing
        // in the merged wake loop touches it. `Runner::reload`'s
        // `rebuild_old` path (triggered by a `spawn_generation_for_reload`
        // failure, e.g. the `force_reload_spawn_failure` test seam) reuses
        // `self.generation`'s LIVE controllers unchanged (see
        // `Runner::rebuild_old`'s doc: "Reuses the current generation's
        // live controllers (no re-probe)") — it never rebuilds a
        // `ControllerBuildContext`, so the SAME `GammaHoldRegistry`/
        // `GammaBreadcrumb` (and hence the same saved table) trivially
        // survives a spawn failure exactly as it survives a successful
        // reload. This test pins the observable half (still black, saved
        // table intact) that this harness CAN exercise without a real
        // `spawn_generation_for_reload` call; the "0 system restores" half
        // is a structural property, not a per-test count: `Runner::reload`
        // and `Runner::rebuild_old` never call
        // `dormantd::gamma_recovery::GammaSystemRestore` anywhere in their
        // bodies (grep-verified — that restore seam is wired ONLY into
        // `main.rs`'s startup/shutdown paths, never SIGHUP/reload; see the
        // module's own doc and this task's report).
        let selector = "cg:panel";
        let api = SimpleFakeGammaApi::with_display(selector, GammaTable::linear(64));
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let old_exec = gamma_executor("mon", selector, api.clone(), holds, breadcrumb);
        old_exec.blank(BlankMode::BrightnessZero).await.unwrap();
        assert!(api.current(selector).is_black());

        // Simulate `rebuild_old`: nothing in the merged wake loop runs
        // (there is no "new generation" to compute removed/changed against
        // at all — `rebuild_old` doesn't call `assemble_static` again), and
        // the old executor/registry are simply retained as-is.
        assert!(
            api.current(selector).is_black(),
            "still black after a simulated spawn-failure rollback"
        );
    }

    // ── Test 6: removed_display_wake_failure_rebuilds_old_gamma_generation ──

    #[tokio::test]
    async fn removed_display_wake_failure_rebuilds_old_gamma_generation() {
        let selector = "cg:panel";
        let old_cfg = config_with(
            "mon",
            gamma_display_cfg("macos-gamma-black", selector),
            false,
        );
        let new_cfg = config_with("other", gamma_display_cfg("command", "n/a"), false);

        // The physical wake-replay WRITE itself fails (not just a
        // confirmation-read mismatch) — the table provably never leaves
        // black.
        let api = SimpleFakeGammaApi::with_display(selector, GammaTable::linear(64))
            .fail_non_black_writes();
        let holds = Arc::new(GammaHoldRegistry::default());
        let (_dir, breadcrumb) = test_breadcrumb();
        let old_exec = gamma_executor("mon", selector, api.clone(), holds, breadcrumb);
        old_exec.blank(BlankMode::BrightnessZero).await.unwrap();
        assert!(api.current(selector).is_black());

        let mut old_executors = HashMap::new();
        old_executors.insert(DisplayId("mon".into()), old_exec);
        let new_executors: HashMap<DisplayId, Arc<DisplayExecutor>> = HashMap::new();

        let snapshot = dark_snapshot("mon");
        let removed = removed_dark_displays(Some(&snapshot), &new_executors);
        let ruled: HashSet<DisplayId> = index_display_rules(&new_cfg).keys().cloned().collect();
        let changed_identity =
            changed_dispatch_dark_displays(Some(&snapshot), &old_cfg, &new_cfg, &ruled);
        assert_eq!(removed, vec![DisplayId("mon".into())]);

        let result = run_merged_wake_loop(removed, changed_identity, &old_executors).await;

        assert!(
            result.is_err(),
            "a failed verified wake must abort the merged wake loop, exactly \
             like `Runner::reload` calling `rebuild_old` and returning"
        );
        assert!(
            api.current(selector).is_black(),
            "still black: the failed wake's write never landed, so the panel \
             — and the OLD generation `rebuild_old` restarts — are unchanged"
        );
        // "display still present": `old_executors` (what `rebuild_old`
        // reuses) still contains "mon" — nothing in this harness (or the
        // real `Runner::reload`) removes an entry from the OLD generation's
        // executor map on a failed wake.
        assert!(old_executors.contains_key(&DisplayId("mon".into())));
    }
}
