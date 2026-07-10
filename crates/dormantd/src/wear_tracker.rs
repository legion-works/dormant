//! Panel-wear tracker: daemon-lifetime task that samples display state on a
//! schedule, attributes brightness-weighted on-time to a per-display
//! [`WearLedger`] grid, persists ledgers to disk, and publishes
//! [`DaemonEvent::WearSnapshot`] / [`DaemonEvent::CompensationAdvisory`]
//! events over the front control channel.
//!
//! ## Split (P6 — a pure fn cannot call async `read_state`)
//!
//! - `collect_samples` — ASYNC SHELL: reads panel state (sampler priority,
//!   bounded by `read_timeout`) for whichever displays are currently in the
//!   `active` phase (the only phase where the attribution table needs a real
//!   brightness reading).
//! - `tick` — GENUINELY PURE: zero I/O, zero tokio. Given the current
//!   [`StateSnapshot`], this round's samples, config, and wall-clock time, it
//!   mutates the in-memory `TrackerState` (attribution, dwell tracking,
//!   advisory latch, persist-due bookkeeping) and returns a list of
//!   `TrackerAction`s for the shell to execute (file I/O, event-bus publish,
//!   usage-hours seeding).
//!
//! Ledger *creation* (loading an existing file, corrupt-file recovery,
//! future-schema-version read-only mode) is impure (file I/O) and lives in
//! `load_or_create_ledger`, called by the shell the first time a display is
//! observed — `tick` itself never touches the filesystem.
//!
//! ## Native brightness scale
//!
//! [`brightness_norm`] needs the controller's native top-of-scale value
//! (100 for DDC/CI, 50 for Samsung port-1516). `WearConfig` carries no
//! per-controller scale (out of scope for this task — see `NATIVE_MAX_DEFAULT`
//! doc comment), so every controller is normalized against a DDC/CI-shaped
//! 0..=100 scale. Samsung's narrower 0..=50 readback would read as
//! under-bright by this heuristic; a follow-up would thread a per-display
//! native-max through `DisplayConfig`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use dormant_core::config::schema::{Config, WearConfig};
use dormant_core::rules::{ControlMsg, DaemonEvent, StageInfo, StateSnapshot};
use dormant_core::traits::{CommandSink, PanelState};
use dormant_core::types::{DisplayId, StageKind};
use dormant_core::wear::{
    PanelType, WEAR_SCHEMA_VERSION, WearHandle, WearIdentity, WearLedger, brightness_norm,
    sanitize_identity_key,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

/// DDC/CI-shaped native brightness top-of-scale, used for every controller
/// until a per-display native-max is plumbed through config (see module
/// docs).
const NATIVE_MAX_DEFAULT: u16 = 100;

/// Dependencies the wear tracker needs, handed in by `app.rs` at daemon
/// startup.
pub struct WearTrackerDeps {
    /// Live config, so the tracker reacts to `[wear]` edits (enable/disable,
    /// interval changes, grid resize) across reloads without a restart.
    pub config_rx: watch::Receiver<Arc<Config>>,
    /// Front ctl channel — rides `forward_ctl`'s `deliver_or_drop` across
    /// generation swaps, exactly like `AppHandle`'s sender.
    pub ctl_tx: mpsc::Sender<ControlMsg>,
    /// Current generation's executor map, republished by `app.rs` on every
    /// install/rollback and emptied immediately before teardown so the
    /// tracker never calls into a dead executor mid-swap.
    pub executors_rx: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    /// Shared ledger map for concurrent readers (IPC/WebUI).
    pub handle: WearHandle,
    /// Daemon-lifetime cancellation token.
    pub cancel: CancellationToken,
}

/// Spawn the wear tracker. Runs until `deps.cancel` fires.
#[must_use]
pub fn spawn(deps: WearTrackerDeps) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(deps))
}

/// Tri-state park/run bookkeeping for the `wear_tracker_started` /
/// `_parked` / `_resumed` log-event literals: `None` until the first tick,
/// then tracks the last-observed `[wear].enabled` value.
type EnabledHistory = Option<bool>;

#[allow(clippy::too_many_lines)]
async fn run(mut deps: WearTrackerDeps) {
    let mut state = TrackerState::default();
    let mut was_enabled: EnabledHistory = None;
    let dir = dormant_core::paths::wear_state_dir();

    let mut interval = new_interval(deps.config_rx.borrow().wear.sample_interval);

    loop {
        tokio::select! {
            () = deps.cancel.cancelled() => break,
            changed = deps.config_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let period = deps.config_rx.borrow().wear.sample_interval;
                interval = new_interval(period);
            }
            _ = interval.tick() => {
                let cfg = deps.config_rx.borrow().clone();

                if !cfg.wear.enabled {
                    if was_enabled != Some(false) {
                        tracing::info!(event = "wear_tracker_parked");
                    }
                    was_enabled = Some(false);
                    continue;
                }
                match was_enabled {
                    None => tracing::info!(event = "wear_tracker_started"),
                    Some(false) => tracing::info!(event = "wear_tracker_resumed"),
                    Some(true) => {}
                }
                was_enabled = Some(true);

                let executors = deps.executors_rx.borrow().clone();
                let Some(snapshot) = request_snapshot(&deps.ctl_tx).await else {
                    continue;
                };

                let now = now_epoch_s();
                ensure_ledgers_loaded(&mut state, &cfg, &executors, &dir, now);

                let samples = collect_samples(&snapshot, &executors, &cfg.wear).await;

                let actions = tick(&mut state, &snapshot, &samples, &cfg.wear, now);
                apply_actions(&mut state, actions, &executors, &deps.ctl_tx, &dir).await;
                sync_handle(&state, &deps.handle);
            }
        }
    }
}

fn new_interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period.max(Duration::from_millis(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

fn now_epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Request a point-in-time [`StateSnapshot`] from the current generation's
/// engine. `None` on a transient miss (no live generation, or the reply
/// raced a reload swap) — the caller simply skips that tick.
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

/// For every display the current generation exposes, resolve its on-disk
/// ledger status exactly once (load / corrupt-recover / future-version /
/// brand-new) and insert the result into `state`.
fn ensure_ledgers_loaded(
    state: &mut TrackerState,
    cfg: &Config,
    executors: &HashMap<DisplayId, Arc<dyn CommandSink>>,
    dir: &Path,
    now_epoch_s: u64,
) {
    for display_id in executors.keys() {
        if state.resolved.contains(display_id) {
            continue;
        }
        state.resolved.insert(display_id.clone());

        let panel_type = cfg
            .displays
            .get(&display_id.0)
            .map(|d| d.panel_type)
            .unwrap_or_default();
        let key = sanitize_identity_key(&display_id.0);
        let identity = WearIdentity {
            key: key.clone(),
            display_name: display_id.0.clone(),
        };
        let result = load_or_create_ledger(
            dir,
            &key,
            identity,
            panel_type,
            cfg.wear.grid_rows,
            cfg.wear.grid_cols,
            now_epoch_s,
        );
        state.ledgers.insert(display_id.clone(), result.ledger);
        if result.needs_seed {
            state.pending_seed.insert(display_id.clone());
        }
        if result.persist_readonly {
            state.persist_readonly.insert(display_id.clone());
        }
    }
}

/// ASYNC SHELL: sample panel state (sampler priority, bounded by
/// `cfg.read_timeout`) only for displays currently in the `active` stage —
/// every other stage uses a fixed attribution factor and needs no hardware
/// read.
async fn collect_samples(
    snapshot: &StateSnapshot,
    executors: &HashMap<DisplayId, Arc<dyn CommandSink>>,
    cfg: &WearConfig,
) -> HashMap<DisplayId, Option<PanelState>> {
    let mut out = HashMap::new();
    for (id_str, dsnap) in &snapshot.displays {
        let display_id = DisplayId(id_str.clone());
        let Some(sink) = executors.get(&display_id) else {
            continue;
        };
        let stage_kind = stage_literal(&dsnap.phase, dsnap.stage.as_ref());
        let sample = if stage_kind == "active" {
            tokio::time::timeout(cfg.read_timeout, sink.read_state_sampled())
                .await
                .unwrap_or(None)
        } else {
            None
        };
        out.insert(display_id, sample);
    }
    out
}

/// Apply the actions a `tick` call returned: file I/O, event-bus publish,
/// and the one-shot usage-hours seed read.
async fn apply_actions(
    state: &mut TrackerState,
    actions: Vec<TrackerAction>,
    executors: &HashMap<DisplayId, Arc<dyn CommandSink>>,
    ctl_tx: &mpsc::Sender<ControlMsg>,
    dir: &Path,
) {
    for action in actions {
        match action {
            TrackerAction::Attribute { .. } => {
                // Already applied to the ledger inside `tick`; nothing left
                // for the shell to do beyond the `sync_handle` pass below.
            }
            TrackerAction::Persist {
                display: display_id,
            } => {
                if state.persist_readonly.contains(&display_id) {
                    continue;
                }
                let Some(ledger) = state.ledgers.get(&display_id) else {
                    continue;
                };
                let key = sanitize_identity_key(&display_id.0);
                if let Err(e) = persist_ledger(dir, &key, ledger) {
                    tracing::warn!(event = "wear_persist_failed", display = %display_id, error = %e);
                }
            }
            TrackerAction::EmitSnapshot {
                display,
                total_on_hours,
                sample_count,
            } => {
                let _ = ctl_tx
                    .send(ControlMsg::PublishDaemonEvent(DaemonEvent::WearSnapshot {
                        display,
                        total_on_hours,
                        sample_count,
                    }))
                    .await;
            }
            TrackerAction::EmitAdvisory {
                display,
                hours_since_long_dwell,
            } => {
                let _ = ctl_tx
                    .send(ControlMsg::PublishDaemonEvent(
                        DaemonEvent::CompensationAdvisory {
                            display,
                            hours_since_long_dwell,
                        },
                    ))
                    .await;
            }
            TrackerAction::Seed {
                display: display_id,
            } => {
                let Some(sink) = executors.get(&display_id) else {
                    continue;
                };
                if let Some(hours) = sink.read_usage_hours().await {
                    if let Some(ledger) = state.ledgers.get_mut(&display_id) {
                        ledger.seeded_usage_hours = Some(hours);
                    }
                    tracing::info!(event = "wear_ledger_seeded", display = %display_id, hours);
                }
            }
        }
    }
}

/// Copy every ledger in `state` into the shared [`WearHandle`] so concurrent
/// readers (IPC/WebUI) see the latest attribution.
fn sync_handle(state: &TrackerState, handle: &WearHandle) {
    let Ok(mut guard) = handle.write() else {
        return;
    };
    for (display_id, ledger) in &state.ledgers {
        guard.insert(sanitize_identity_key(&display_id.0), ledger.clone());
    }
}

// ── Pure core ────────────────────────────────────────────────────────────────

/// In-memory tracker bookkeeping, carried across ticks. Contains no shared
/// (`Arc`/lock) state — the async shell owns one `TrackerState` and
/// periodically syncs its ledgers into the shared [`WearHandle`].
#[derive(Default)]
struct TrackerState {
    ledgers: HashMap<DisplayId, WearLedger>,
    /// Displays whose on-disk status has already been resolved (loaded,
    /// recovered, or confirmed absent) — the shell only touches the
    /// filesystem once per display.
    resolved: HashSet<DisplayId>,
    /// Freshly created (no prior file) ledgers awaiting a one-shot
    /// `read_usage_hours()` seed.
    pending_seed: HashSet<DisplayId>,
    /// Displays whose on-disk ledger is a newer schema version than this
    /// build understands — never persist over that file.
    persist_readonly: HashSet<DisplayId>,
    /// In-memory advisory latch (NOT persisted): true once an advisory has
    /// fired for the current baseline/observed window, cleared on the next
    /// qualifying long dwell.
    advisory_active: HashMap<DisplayId, bool>,
    /// Epoch-seconds the display's current dark span started, if any.
    dwell_start: HashMap<DisplayId, Option<u64>>,
    /// Epoch-seconds of the last successful persist, per display.
    last_persist_epoch_s: HashMap<DisplayId, u64>,
}

/// One action the pure [`tick`] wants the async shell to perform.
#[derive(Debug, Clone, PartialEq)]
enum TrackerAction {
    /// Attribution was applied to `display`'s ledger (informational — the
    /// mutation already happened inside `tick`; shell/tests use this to
    /// observe what happened).
    Attribute {
        display: DisplayId,
        span: Duration,
        norm: f64,
    },
    /// Persist `display`'s ledger to disk now.
    Persist { display: DisplayId },
    /// Publish a `WearSnapshot` event for `display`.
    EmitSnapshot {
        display: DisplayId,
        total_on_hours: f64,
        sample_count: u64,
    },
    /// Publish a `CompensationAdvisory` event for `display`.
    EmitAdvisory {
        display: DisplayId,
        hours_since_long_dwell: u64,
    },
    /// Seed `display`'s freshly created ledger with `read_usage_hours()`.
    Seed { display: DisplayId },
}

/// Classify a display's effective wear-attribution stage from its
/// [`StateSnapshot`] phase + active ladder stage.
fn stage_literal(phase: &str, stage: Option<&StageInfo>) -> &'static str {
    if let Some(s) = stage {
        return match s.kind {
            StageKind::RenderScreensaver => "render_screensaver",
            StageKind::RenderBlack => "render_black",
            StageKind::Controller(_) => "blanked",
        };
    }
    match phase {
        "active" => "active",
        "blanked" => "blanked",
        _ => "unknown",
    }
}

/// Pure tracker tick: given the current snapshot/samples/config, mutate
/// `state`'s ledgers and bookkeeping in place and return the actions the
/// shell must execute. Zero I/O, zero tokio — see module docs.
#[allow(clippy::too_many_lines)]
fn tick(
    state: &mut TrackerState,
    snapshot: &StateSnapshot,
    samples: &HashMap<DisplayId, Option<PanelState>>,
    cfg: &WearConfig,
    now_epoch_s: u64,
) -> Vec<TrackerAction> {
    let mut actions = Vec::new();
    if !cfg.enabled {
        return actions;
    }

    let sample_interval_s = cfg.sample_interval.as_secs().max(1);
    let max_span_s = sample_interval_s.saturating_mul(2);
    let persist_interval_s = cfg.persist_interval.as_secs().max(1);
    let short_cycle_s = cfg.short_cycle_dwell.as_secs();
    let advisory_after_s = cfg.advisory_after.as_secs();

    for (id_str, dsnap) in &snapshot.displays {
        let display_id = DisplayId(id_str.clone());
        let Some(ledger) = state.ledgers.get_mut(&display_id) else {
            continue;
        };

        // Grid resize on config change.
        if ledger.grid_rows != cfg.grid_rows || ledger.grid_cols != cfg.grid_cols {
            ledger.resize_grid(cfg.grid_rows, cfg.grid_cols);
        }

        // Seed (once) — the shell populated `pending_seed` when it created
        // this ledger fresh (no prior file on disk).
        if state.pending_seed.remove(&display_id) {
            actions.push(TrackerAction::Seed {
                display: display_id.clone(),
            });
        }

        let stage_kind = stage_literal(&dsnap.phase, dsnap.stage.as_ref());

        // ── Attribution ──────────────────────────────────────────────────
        let elapsed_s = ledger
            .last_sample_at_epoch_s
            .map_or(sample_interval_s, |last| now_epoch_s.saturating_sub(last));
        let span_s = elapsed_s.min(max_span_s);
        let span = Duration::from_secs(span_s);

        let norm = match stage_kind {
            "render_screensaver" => cfg.screensaver_factor.clamp(0.0, 1.0),
            "render_black" | "blanked" => 0.0,
            "active" => {
                let sample = samples.get(&display_id).cloned().flatten();
                if sample.is_none() {
                    tracing::debug!(event = "wear_sample_fallback", display = %display_id);
                }
                brightness_norm(
                    &sample.unwrap_or_default(),
                    NATIVE_MAX_DEFAULT,
                    cfg.fallback_brightness,
                )
            }
            _ => cfg.fallback_brightness.clamp(0.0, 1.0),
        };

        ledger.attribute_uniform(span, norm);
        ledger.last_sample_at_epoch_s = Some(now_epoch_s);
        actions.push(TrackerAction::Attribute {
            display: display_id.clone(),
            span,
            norm,
        });

        // ── Dwell tracking (dark = render_black stage or blanked phase) ───
        let is_dark = matches!(stage_kind, "render_black" | "blanked");
        let dwell_entry = state.dwell_start.entry(display_id.clone()).or_insert(None);
        match (is_dark, *dwell_entry) {
            (true, None) => *dwell_entry = Some(now_epoch_s),
            (false, Some(start)) => {
                let dwell_s = now_epoch_s.saturating_sub(start);
                if dwell_s >= short_cycle_s {
                    ledger.last_long_dwell_epoch_s = Some(now_epoch_s);
                    state.advisory_active.insert(display_id.clone(), false);
                }
                *dwell_entry = None;
            }
            (true, Some(_)) | (false, None) => {}
        }

        // ── Advisory (observed vs baseline, ONCE latch) ────────────────────
        let baseline = ledger.advisory_baseline_epoch_s;
        let observed = ledger.last_long_dwell_epoch_s.unwrap_or(0);
        let reference = observed.max(baseline);
        let since_s = now_epoch_s.saturating_sub(reference);
        let already_active = state
            .advisory_active
            .get(&display_id)
            .copied()
            .unwrap_or(false);
        if since_s > advisory_after_s && !already_active {
            tracing::info!(event = "wear_advisory", display = %display_id, hours = since_s / 3600);
            actions.push(TrackerAction::EmitAdvisory {
                display: display_id.clone(),
                hours_since_long_dwell: since_s / 3600,
            });
            state.advisory_active.insert(display_id.clone(), true);
        }

        // ── Persist / snapshot cadence ──────────────────────────────────────
        let last_persist = state
            .last_persist_epoch_s
            .get(&display_id)
            .copied()
            .unwrap_or(0);
        if now_epoch_s.saturating_sub(last_persist) >= persist_interval_s {
            actions.push(TrackerAction::Persist {
                display: display_id.clone(),
            });
            actions.push(TrackerAction::EmitSnapshot {
                display: display_id.clone(),
                total_on_hours: ledger.total_on_hours,
                sample_count: ledger.sample_count,
            });
            state
                .last_persist_epoch_s
                .insert(display_id.clone(), now_epoch_s);
        }
    }

    actions
}

// ── Impure ledger load/create/persist (file I/O — the shell's job) ─────────────

/// Outcome of resolving a display's on-disk ledger status the first time the
/// tracker sees it.
struct LoadResult {
    ledger: WearLedger,
    /// `true` only when there was NO prior file at all (brand new panel) —
    /// corrupt-recovery and future-version fresh-starts do NOT re-seed
    /// (the panel was already being tracked, just unreadable).
    needs_seed: bool,
    /// `true` when the on-disk file must never be overwritten (future
    /// schema version, or a corrupt-file rename that itself failed).
    persist_readonly: bool,
}

/// Load `<dir>/wear-<key>.json`, or create a fresh ledger if absent,
/// corrupt, or a future schema version. See spec §5.2 / §3.1. Pure math
/// aside, this is impure (filesystem) — the shell calls it once per display,
/// the first time that display is observed.
fn load_or_create_ledger(
    dir: &Path,
    key: &str,
    identity: WearIdentity,
    panel_type: PanelType,
    rows: u16,
    cols: u16,
    now_epoch_s: u64,
) -> LoadResult {
    let path = dir.join(format!("wear-{key}.json"));
    let display_key = identity.key.clone();

    let Ok(contents) = std::fs::read_to_string(&path) else {
        // Absent (or otherwise unreadable, e.g. permissions) — brand new.
        return LoadResult {
            ledger: WearLedger::new(identity, panel_type, rows, cols, now_epoch_s),
            needs_seed: true,
            persist_readonly: false,
        };
    };

    match serde_json::from_str::<WearLedger>(&contents) {
        Ok(ledger) if ledger.schema_version > WEAR_SCHEMA_VERSION => {
            tracing::warn!(
                event = "wear_ledger_future_version",
                display = %display_key,
                on_disk_version = ledger.schema_version,
                supported_version = WEAR_SCHEMA_VERSION,
            );
            LoadResult {
                ledger: WearLedger::new(identity, panel_type, rows, cols, now_epoch_s),
                needs_seed: false,
                persist_readonly: true,
            }
        }
        Ok(ledger) => LoadResult {
            ledger,
            needs_seed: false,
            persist_readonly: false,
        },
        Err(_) => {
            let corrupt_path = dir.join(format!("wear-{key}.json.corrupt.{now_epoch_s}"));
            match std::fs::rename(&path, &corrupt_path) {
                Ok(()) => {
                    tracing::warn!(event = "wear_ledger_corrupt", display = %display_key);
                    LoadResult {
                        ledger: WearLedger::new(identity, panel_type, rows, cols, now_epoch_s),
                        needs_seed: false,
                        persist_readonly: false,
                    }
                }
                Err(e) => {
                    tracing::warn!(event = "wear_persist_failed", display = %display_key, error = %e);
                    LoadResult {
                        ledger: WearLedger::new(identity, panel_type, rows, cols, now_epoch_s),
                        needs_seed: false,
                        persist_readonly: true,
                    }
                }
            }
        }
    }
}

/// Atomically persist `ledger` to `<dir>/wear-<key>.json` (tmp+rename, same
/// dir, mode 0644 on Unix). Creates `dir` if missing.
fn persist_ledger(dir: &Path, key: &str, ledger: &WearLedger) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let final_path = dir.join(format!("wear-{key}.json"));
    let tmp_path = dir.join(format!("wear-{key}.json.tmp"));
    let json = serde_json::to_string(ledger)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "test literals are exact by construction (0.0 attribution factors, direct \
              round-trip of an unmodified f64 field) — no accumulated float error to tolerate"
)]
mod tests {
    use super::*;
    use dormant_core::rules::DisplaySnapshot;

    fn fresh_ledger(display: &DisplayId, now: u64) -> WearLedger {
        WearLedger::new(
            WearIdentity {
                key: sanitize_identity_key(&display.0),
                display_name: display.0.clone(),
            },
            PanelType::Unknown,
            9,
            16,
            now,
        )
    }

    fn snapshot_with(display: &DisplayId, phase: &str, stage: Option<StageInfo>) -> StateSnapshot {
        StateSnapshot {
            sensors: Vec::new(),
            zones: Vec::new(),
            displays: vec![(
                display.0.clone(),
                DisplaySnapshot {
                    phase: phase.to_string(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    controllers: Vec::new(),
                    stage,
                },
            )],
            pending_reload: None,
        }
    }

    fn find_attribute(actions: &[TrackerAction], display: &DisplayId) -> Option<(Duration, f64)> {
        actions.iter().find_map(|a| match a {
            TrackerAction::Attribute {
                display: d,
                span,
                norm,
            } if d == display => Some((*span, *norm)),
            _ => None,
        })
    }

    #[test]
    fn active_phase_attributes_with_read_brightness() {
        let display = DisplayId("mon".into());
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        let snapshot = snapshot_with(&display, "active", None);
        let mut samples = HashMap::new();
        samples.insert(
            display.clone(),
            Some(PanelState {
                power: None,
                brightness: Some(80),
            }),
        );
        let cfg = WearConfig::default();
        let actions = tick(&mut state, &snapshot, &samples, &cfg, 1_000_060);
        let (_, norm) = find_attribute(&actions, &display).expect("Attribute action");
        assert!((norm - 0.8).abs() < 1e-9);
    }

    #[test]
    fn staged_screensaver_uses_factor() {
        let display = DisplayId("mon".into());
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        let snapshot = snapshot_with(
            &display,
            "staged",
            Some(StageInfo {
                idx: 0,
                kind: StageKind::RenderScreensaver,
            }),
        );
        let samples = HashMap::new();
        let cfg = WearConfig::default();
        let actions = tick(&mut state, &snapshot, &samples, &cfg, 1_000_060);
        let (_, norm) = find_attribute(&actions, &display).expect("Attribute action");
        assert!((norm - cfg.screensaver_factor).abs() < 1e-9);
    }

    #[test]
    fn staged_black_and_blanked_attribute_zero() {
        let cfg = WearConfig::default();

        let display = DisplayId("black".into());
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        let snapshot = snapshot_with(
            &display,
            "staged",
            Some(StageInfo {
                idx: 0,
                kind: StageKind::RenderBlack,
            }),
        );
        let actions = tick(&mut state, &snapshot, &HashMap::new(), &cfg, 1_000_060);
        let (_, norm) = find_attribute(&actions, &display).expect("Attribute action");
        assert_eq!(norm, 0.0);

        let display2 = DisplayId("blanked".into());
        let mut state2 = TrackerState::default();
        state2
            .ledgers
            .insert(display2.clone(), fresh_ledger(&display2, 0));
        let snapshot2 = snapshot_with(&display2, "blanked", None);
        let actions2 = tick(&mut state2, &snapshot2, &HashMap::new(), &cfg, 1_000_060);
        let (_, norm2) = find_attribute(&actions2, &display2).expect("Attribute action");
        assert_eq!(norm2, 0.0);
    }

    #[test]
    fn unknown_phase_falls_back() {
        let display = DisplayId("mon".into());
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        let snapshot = snapshot_with(&display, "grace", None);
        let cfg = WearConfig::default();
        let actions = tick(&mut state, &snapshot, &HashMap::new(), &cfg, 1_000_060);
        let (_, norm) = find_attribute(&actions, &display).expect("Attribute action");
        assert!((norm - cfg.fallback_brightness).abs() < 1e-9);
    }

    #[test]
    fn span_clamped_after_gap() {
        let display = DisplayId("mon".into());
        let cfg = WearConfig::default();
        let mut ledger = fresh_ledger(&display, 0);
        ledger.last_sample_at_epoch_s = Some(0);
        let mut state = TrackerState::default();
        state.ledgers.insert(display.clone(), ledger);
        let now = cfg.sample_interval.as_secs() * 10;
        let snapshot = snapshot_with(&display, "active", None);
        let mut samples = HashMap::new();
        samples.insert(
            display.clone(),
            Some(PanelState {
                power: None,
                brightness: Some(100),
            }),
        );
        let actions = tick(&mut state, &snapshot, &samples, &cfg, now);
        let (span, _) = find_attribute(&actions, &display).expect("Attribute action");
        assert_eq!(span, Duration::from_secs(cfg.sample_interval.as_secs() * 2));
    }

    #[test]
    fn dwell_edge_records_observed_and_clears_advisory() {
        let display = DisplayId("mon".into());
        let cfg = WearConfig::default();
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        state.dwell_start.insert(display.clone(), Some(0));
        state.advisory_active.insert(display.clone(), true);
        let now = 11 * 60; // 11 minutes dark — over the 10m short_cycle_dwell default
        let snapshot = snapshot_with(&display, "active", None);
        let mut samples = HashMap::new();
        samples.insert(
            display.clone(),
            Some(PanelState {
                power: None,
                brightness: Some(100),
            }),
        );
        let _ = tick(&mut state, &snapshot, &samples, &cfg, now);
        let ledger = state.ledgers.get(&display).unwrap();
        assert_eq!(ledger.last_long_dwell_epoch_s, Some(now));
        assert_eq!(state.advisory_active.get(&display).copied(), Some(false));
    }

    #[test]
    fn short_dwell_not_recorded() {
        let display = DisplayId("mon".into());
        let cfg = WearConfig::default();
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        state.dwell_start.insert(display.clone(), Some(0));
        let now = 3 * 60; // 3 minutes — under the 10m default
        let snapshot = snapshot_with(&display, "active", None);
        let mut samples = HashMap::new();
        samples.insert(
            display.clone(),
            Some(PanelState {
                power: None,
                brightness: Some(100),
            }),
        );
        let _ = tick(&mut state, &snapshot, &samples, &cfg, now);
        let ledger = state.ledgers.get(&display).unwrap();
        assert_eq!(ledger.last_long_dwell_epoch_s, None);
    }

    #[test]
    fn advisory_emitted_once_from_baseline() {
        let display = DisplayId("mon".into());
        let cfg = WearConfig::default();
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        let now = cfg.advisory_after.as_secs() + 1;
        let snapshot = snapshot_with(&display, "active", None);
        let mut samples = HashMap::new();
        samples.insert(
            display.clone(),
            Some(PanelState {
                power: None,
                brightness: Some(100),
            }),
        );

        let actions1 = tick(&mut state, &snapshot, &samples, &cfg, now);
        let count1 = actions1
            .iter()
            .filter(|a| matches!(a, TrackerAction::EmitAdvisory { .. }))
            .count();
        assert_eq!(count1, 1);

        let actions2 = tick(&mut state, &snapshot, &samples, &cfg, now + 1);
        let count2 = actions2
            .iter()
            .filter(|a| matches!(a, TrackerAction::EmitAdvisory { .. }))
            .count();
        assert_eq!(count2, 0);
    }

    #[test]
    fn disabled_produces_no_actions() {
        let display = DisplayId("mon".into());
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0));
        let snapshot = snapshot_with(&display, "active", None);
        let cfg = WearConfig {
            enabled: false,
            ..WearConfig::default()
        };
        let actions = tick(&mut state, &snapshot, &HashMap::new(), &cfg, 1000);
        assert!(actions.is_empty());
    }

    #[test]
    fn grid_resize_applies_when_config_dims_differ() {
        let display = DisplayId("mon".into());
        let mut state = TrackerState::default();
        state
            .ledgers
            .insert(display.clone(), fresh_ledger(&display, 0)); // 9x16
        let snapshot = snapshot_with(&display, "active", None);
        let mut samples = HashMap::new();
        samples.insert(
            display.clone(),
            Some(PanelState {
                power: None,
                brightness: Some(100),
            }),
        );
        let cfg = WearConfig {
            grid_rows: 4,
            grid_cols: 4,
            ..WearConfig::default()
        };
        let _ = tick(&mut state, &snapshot, &samples, &cfg, 1_000_060);
        let ledger = state.ledgers.get(&display).unwrap();
        assert_eq!((ledger.grid_rows, ledger.grid_cols), (4, 4));
    }

    // ── Step 5: persistence (tempdir) ────────────────────────────────────────

    #[test]
    fn persist_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let identity = WearIdentity {
            key: "mon".into(),
            display_name: "mon".into(),
        };
        let mut ledger = WearLedger::new(identity.clone(), PanelType::QdOled, 9, 16, 0);
        ledger.attribute_uniform(Duration::from_secs(3600), 0.5);
        persist_ledger(dir.path(), "mon", &ledger).unwrap();

        let result =
            load_or_create_ledger(dir.path(), "mon", identity, PanelType::QdOled, 9, 16, 100);
        assert!(
            !result.needs_seed,
            "loaded from an existing file — must not re-seed"
        );
        assert!(!result.persist_readonly);
        assert_eq!(result.ledger.total_on_hours, ledger.total_on_hours);
        assert_eq!(result.ledger.schema_version, WEAR_SCHEMA_VERSION);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("wear-mon.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644);
        }
    }

    #[test]
    fn corrupt_file_renamed_aside_and_fresh_started() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wear-mon.json");
        std::fs::write(&path, "{ not json").unwrap();
        let identity = WearIdentity {
            key: "mon".into(),
            display_name: "mon".into(),
        };
        let now = 12_345;
        let result =
            load_or_create_ledger(dir.path(), "mon", identity, PanelType::Unknown, 9, 16, now);
        assert_eq!(result.ledger.total_on_hours, 0.0);
        assert!(!result.needs_seed, "corrupt-recovery must not re-seed");
        assert!(!result.persist_readonly);
        let corrupt_path = dir.path().join(format!("wear-mon.json.corrupt.{now}"));
        assert!(corrupt_path.exists(), "corrupt file must be renamed aside");
        assert!(!path.exists(), "original corrupt path must be gone");
    }

    #[test]
    fn corrupt_rename_failure_marks_persist_readonly_and_skips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wear-mon.json");
        std::fs::write(&path, "{ not json").unwrap();
        let now = 999;
        // Pre-create the corrupt-target path as a DIRECTORY so the rename
        // deterministically fails (EISDIR), regardless of process uid.
        std::fs::create_dir(dir.path().join(format!("wear-mon.json.corrupt.{now}"))).unwrap();
        let identity = WearIdentity {
            key: "mon".into(),
            display_name: "mon".into(),
        };
        let result =
            load_or_create_ledger(dir.path(), "mon", identity, PanelType::Unknown, 9, 16, now);
        assert!(
            result.persist_readonly,
            "rename failure must mark persist-readonly (skip persist for this display)"
        );
        assert!(!result.needs_seed);
    }

    #[test]
    fn future_schema_version_loads_read_only_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let identity = WearIdentity {
            key: "mon".into(),
            display_name: "mon".into(),
        };
        let mut future = WearLedger::new(identity.clone(), PanelType::Unknown, 9, 16, 0);
        future.schema_version = 99;
        let path = dir.path().join("wear-mon.json");
        let original = serde_json::to_string(&future).unwrap();
        std::fs::write(&path, &original).unwrap();

        let result =
            load_or_create_ledger(dir.path(), "mon", identity, PanelType::Unknown, 9, 16, 500);
        assert!(result.persist_readonly);
        assert!(
            !result.needs_seed,
            "an existing (if future) file must not seed"
        );
        assert_eq!(result.ledger.schema_version, WEAR_SCHEMA_VERSION);

        let mut ledger = result.ledger;
        ledger.attribute_uniform(Duration::from_secs(3600), 1.0);
        if !result.persist_readonly {
            persist_ledger(dir.path(), "mon", &ledger).unwrap();
        }
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after, original,
            "future-version file must never be overwritten"
        );
    }

    #[test]
    fn seed_action_only_when_ledger_absent() {
        let dir = tempfile::tempdir().unwrap();
        let identity = WearIdentity {
            key: "mon".into(),
            display_name: "mon".into(),
        };
        // Brand new — no file — needs_seed true.
        let fresh = load_or_create_ledger(
            dir.path(),
            "mon",
            identity.clone(),
            PanelType::Unknown,
            9,
            16,
            0,
        );
        assert!(fresh.needs_seed);
        persist_ledger(dir.path(), "mon", &fresh.ledger).unwrap();

        // Second load, file now exists — needs_seed false.
        let again =
            load_or_create_ledger(dir.path(), "mon", identity, PanelType::Unknown, 9, 16, 100);
        assert!(!again.needs_seed);
    }
}
