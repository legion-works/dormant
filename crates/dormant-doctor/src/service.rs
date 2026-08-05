//! `DoctorService` — live, coalesced doctor runs against a running daemon.
//!
//! The CLI's offline `dormantctl doctor` probes the hardware cold (it must,
//! because no daemon is running).  The daemon's online path is different:
//! the daemon already holds the USB serial port and the DDC/CI bus, so a
//! second open would race (EBUSY) or steal the handle.  This service
//! reports OWNED devices from the live [`StateSnapshot`] (via
//! [`ControlMsg::Snapshot`]) and only actively probes the NON-exclusive
//! network services (MQTT broker reachability, Home Assistant WebSocket).
//!
//! ## Singleflight
//!
//! Concurrent `run()` calls share the ONE in-flight run: each caller
//! receives the same [`DoctorReport`].  The future is stored as
//! `Weak<Shared<…>>` so the slot self-cleans when the last caller drops
//! its reference, and a fresh run starts on the next call after that.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::{Mutex, mpsc, watch};

use dormant_core::config::schema::{Config, Credentials, SensorConfig};
use dormant_core::doctor::{Check, CheckStatus, DoctorReport};
use dormant_core::rules::{ControlMsg, StateSnapshot};
use dormant_core::types::SensorState;
use dormant_core::wear::WearSamplingStatus;

use crate::types::{ProbeResult, ProbeStatus};

type SharedRun = futures_util::future::Shared<Pin<Box<dyn Future<Output = DoctorReport> + Send>>>;

/// Live, coalesced doctor service.
///
/// Cloneable (Arc-backed) so the IPC server and the web server can share one
/// instance without duplicating probe work.
#[derive(Clone)]
pub struct DoctorService {
    inner: Arc<Inner>,
}

struct Inner {
    /// Clone of the engine's `ControlMsg` channel — used to fetch a live
    /// snapshot via `ControlMsg::Snapshot(oneshot)`.
    ctl_tx: mpsc::Sender<ControlMsg>,
    /// Live config watch (read-only receiver).
    config_rx: watch::Receiver<Arc<Config>>,
    /// Live credentials watch (read-only receiver).
    creds_rx: watch::Receiver<Arc<Credentials>>,
    /// Redacted sampler status published by the daemon's sole sampler owner.
    sampler_status_rx: Option<watch::Receiver<Option<WearSamplingStatus>>>,
    /// Per-display redacted sampler statuses (issue #185 cycle B). The
    /// daemon's registry of samplers writes one entry per active sampler;
    /// the doctor emits ONE wear-sampling check PER configured display
    /// from this map.  `None` when the daemon has no active sampler
    /// registry (legacy single-display builds, tests) — the per-display
    /// path then falls back to the singular probe so the existing
    /// contract survives.
    sampler_statuses_rx: Option<watch::Receiver<BTreeMap<String, WearSamplingStatus>>>,
    /// Coalesce slot: weak handle to the in-flight run, if any.
    inflight: Mutex<Option<Weak<SharedRun>>>,
}

/// Maximum time the snapshot fetch is allowed to take before we fall back
/// to an empty snapshot (engine unresponsive).
const SNAPSHOT_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time an active network probe (MQTT, HA) is allowed to take per
/// sensor.  Probes are spawned concurrently; this caps the wall clock cost
/// of the whole `run()`.
const NETWORK_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

impl DoctorService {
    /// Build a new service.  The receivers are cloned internally — callers
    /// keep their own handles for other uses (config view, web UI).
    #[must_use]
    pub fn new(
        ctl_tx: mpsc::Sender<ControlMsg>,
        config_rx: watch::Receiver<Arc<Config>>,
        creds_rx: watch::Receiver<Arc<Credentials>>,
    ) -> Self {
        Self::new_with_sampler_statuses(ctl_tx, config_rx, creds_rx, None, None)
    }

    /// Build a service with the daemon-owned, redacted sampler status watch.
    #[must_use]
    pub fn new_with_sampler_status(
        ctl_tx: mpsc::Sender<ControlMsg>,
        config_rx: watch::Receiver<Arc<Config>>,
        creds_rx: watch::Receiver<Arc<Credentials>>,
        sampler_status_rx: Option<watch::Receiver<Option<WearSamplingStatus>>>,
    ) -> Self {
        Self::new_with_sampler_statuses(ctl_tx, config_rx, creds_rx, sampler_status_rx, None)
    }

    /// Build a service with both the singular and per-display sampler
    /// status watches (issue #185 cycle B).  Both are optional so
    /// callers can pass either, neither, or both.
    #[must_use]
    pub fn new_with_sampler_statuses(
        ctl_tx: mpsc::Sender<ControlMsg>,
        config_rx: watch::Receiver<Arc<Config>>,
        creds_rx: watch::Receiver<Arc<Credentials>>,
        sampler_status_rx: Option<watch::Receiver<Option<WearSamplingStatus>>>,
        sampler_statuses_rx: Option<watch::Receiver<BTreeMap<String, WearSamplingStatus>>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                ctl_tx,
                config_rx,
                creds_rx,
                sampler_status_rx,
                sampler_statuses_rx,
                inflight: Mutex::new(None),
            }),
        }
    }

    /// Run the doctor; coalesce concurrent calls onto the same in-flight
    /// report.
    ///
    /// Reports:
    /// - **Owned sensors (USB)**: `Skip` — daemon holds the serial port,
    ///   cold probe would re-open.  Detail carries the live snapshot state.
    /// - **Owned displays (DDC/CI, `KWin`, ...)**: per-controller `Ok`/`Fail`
    ///   from the snapshot's `controllers` health (no re-probe).
    /// - **Non-exclusive network sensors (MQTT, HA)**: actively probed
    ///   with a per-sensor timeout; result from the probe.
    /// - **Unsupported controllers** (`samsung-tizen`, `ha-passthrough`,
    ///   `command`): `NotSupported` in this build of the doctor.
    pub async fn run(&self) -> DoctorReport {
        // Critical: never hold the `MutexGuard` across an `.await`.  We
        // acquire, inspect, possibly replace, then drop — the actual
        // `await` happens on the cloned `Shared` future.
        let mut guard = self.inner.inflight.lock().await;

        if let Some(weak) = guard.as_ref()
            && let Some(arc) = weak.upgrade()
        {
            // `arc` is `Arc<SharedRun>`.  `SharedRun: Clone`, so clone
            // the inner Shared out — that owned value IS the future.
            let shared: SharedRun = (*arc).clone();
            drop(guard);
            return shared.await;
        }

        // Start a new run.  The boxed future captures clones of the
        // channel/watch receivers so it owns everything it needs.
        let ctl_tx = self.inner.ctl_tx.clone();
        let config_rx = self.inner.config_rx.clone();
        let creds_rx = self.inner.creds_rx.clone();
        let sampler_status_rx = self.inner.sampler_status_rx.clone();
        let sampler_statuses_rx = self.inner.sampler_statuses_rx.clone();
        let fut: Pin<Box<dyn Future<Output = DoctorReport> + Send>> = Box::pin(run_inner(
            ctl_tx,
            config_rx,
            creds_rx,
            sampler_status_rx,
            sampler_statuses_rx,
        ));
        let shared: SharedRun = fut.shared();
        let arc = Arc::new(shared);
        *guard = Some(Arc::downgrade(&arc));
        // Clone the inner Shared out of the local Arc so we own a
        // future to await; the Arc still lives until the end of the
        // function (keeps the slot upgradeable for concurrent callers).
        let shared: SharedRun = (*arc).clone();
        drop(guard);
        shared.await
    }
}

/// One doctor run: fetch snapshot, build owned-device checks, probe
/// non-exclusive network services.  See [`DoctorService::run`].
#[allow(clippy::too_many_lines)]
async fn run_inner(
    ctl_tx: mpsc::Sender<ControlMsg>,
    config_rx: watch::Receiver<Arc<Config>>,
    creds_rx: watch::Receiver<Arc<Credentials>>,
    sampler_status_rx: Option<watch::Receiver<Option<WearSamplingStatus>>>,
    sampler_statuses_rx: Option<watch::Receiver<BTreeMap<String, WearSamplingStatus>>>,
) -> DoctorReport {
    let snapshot = fetch_snapshot(&ctl_tx).await;
    let cfg = config_rx.borrow().clone();
    let creds = creds_rx.borrow().clone();

    let mut checks: Vec<Check> = Vec::new();

    // Per-display wear-sampling probe (issue #185 cycle B). When the
    // daemon supplies the per-display status map we emit one check
    // per configured display so the operator sees each sampling
    // display's health individually. When the daemon does NOT supply
    // it (legacy / test / off-Linux) we fall back to the singular
    // probe so the existing single-display contract survives.
    if let Some(rx) = sampler_statuses_rx.as_ref() {
        let statuses = rx.borrow().clone();
        let configured = cfg.wear.active_sampling.selected_displays();
        let live: Vec<String> = snapshot.displays.iter().map(|(id, _)| id.clone()).collect();
        let per_display_results = crate::probes::wear_sampling::probe_wear_sampling_per_display(
            &cfg.wear,
            &configured,
            &live,
            &statuses,
        );
        for result in per_display_results {
            checks.push(probe_result_to_check(&result));
        }
    } else {
        let sampler_status = sampler_status_rx.as_ref().map(|rx| rx.borrow().clone());
        let sampler_result = crate::probes::wear_sampling::probe_wear_sampling(
            &cfg.wear,
            cfg.wear
                .active_sampling
                .first_sampled_display()
                .is_some_and(|display| cfg.displays.contains_key(display)),
            sampler_status.as_ref().and_then(Option::as_ref),
        );
        let mut sampler_check = probe_result_to_check(&sampler_result);
        sampler_check.category = Some("platform".into());
        checks.push(sampler_check);
    }

    // ── Owned sensors (USB) — report from snapshot, never re-open ──
    for sensor in &snapshot.sensors {
        let Some(sensor_cfg) = cfg.sensors.get(&sensor.id) else {
            continue;
        };
        match sensor_cfg {
            SensorConfig::UsbLd2410(usb) => {
                let name = format!("usb {}", usb.port);
                let detail = format!(
                    "owned by daemon — see live status (state: {}, last seen: {}s ago)",
                    sensor_state_str(sensor.state),
                    sensor.last_seen_secs_ago,
                );
                checks.push(Check {
                    name,
                    status: CheckStatus::Skip,
                    detail: Some(detail),
                    category: Some("sensor".into()),
                    subject: Some(sensor.id.clone()),
                });
            }
            // MQTT/HA are NOT owned — handled below by active probe.
            SensorConfig::Mqtt(_) | SensorConfig::Ha(_) => {}
        }
    }

    // ── Owned displays — per-controller health from snapshot ──
    for (display_id, display) in &snapshot.displays {
        if display.controllers.is_empty() {
            // No attempts yet (e.g. cold start, or display never blanked).
            // Report a single Skip so the operator sees the display exists
            // but has no health record.
            checks.push(Check {
                name: format!("display {display_id}"),
                status: CheckStatus::Skip,
                detail: Some("owned by daemon — no blank/wake attempts yet".into()),
                category: Some("display".into()),
                subject: Some(display_id.clone()),
            });
            continue;
        }
        for h in &display.controllers {
            let name = format!("{} ({display_id})", h.name);
            let status = if h.healthy {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            };
            let detail = h.detail.clone().or_else(|| {
                if h.healthy {
                    Some("last attempt succeeded".into())
                } else {
                    Some("last attempt failed (no detail recorded)".into())
                }
            });
            checks.push(Check {
                name,
                status,
                detail,
                category: Some("display".into()),
                subject: Some(display_id.clone()),
            });
        }
    }

    // ── macOS-only read-only platform checks — never re-run owned
    // display blank/wake (that stays exclusively under the `Exercise`
    // control path); see `push_macos_platform_checks` (extracted to its own
    // function to keep this one under clippy::too_many_lines).
    #[cfg(target_os = "macos")]
    push_macos_platform_checks(&mut checks, &cfg.input_filter.ignore_devices).await;

    // ── Non-exclusive network sensors (MQTT / HA) — active probe ──
    let mut probe_futs: Vec<std::pin::Pin<Box<dyn Future<Output = Check> + Send>>> = Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        match sensor_cfg {
            SensorConfig::Mqtt(mqtt_cfg) => {
                let id = id.clone();
                let cfg = mqtt_cfg.clone();
                let creds = creds.clone();
                probe_futs.push(Box::pin(async move {
                    let res = tokio::time::timeout(
                        NETWORK_PROBE_TIMEOUT,
                        crate::probes::mqtt::probe_mqtt_one(&id, &cfg, &creds),
                    )
                    .await
                    .unwrap_or_else(|_| ProbeResult::fail(format!("mqtt {id}"), "probe timeout"));
                    let mut check = probe_result_to_check(&res);
                    check.category = Some("sensor".into());
                    check.subject = Some(id.clone());
                    check
                }));
            }
            SensorConfig::Ha(ha_cfg) => {
                let id = id.clone();
                let cfg = ha_cfg.clone();
                let creds = creds.clone();
                probe_futs.push(Box::pin(async move {
                    let res = tokio::time::timeout(
                        NETWORK_PROBE_TIMEOUT,
                        crate::probes::ha::probe_ha_one(&id, &cfg, &creds),
                    )
                    .await
                    .unwrap_or_else(|_| ProbeResult::fail(format!("ha {id}"), "probe timeout"));
                    let mut check = probe_result_to_check(&res);
                    check.category = Some("sensor".into());
                    check.subject = Some(id.clone());
                    check
                }));
            }
            SensorConfig::UsbLd2410(_) => {
                // Already reported as Skip above.
            }
        }
    }

    if !probe_futs.is_empty() {
        let results = futures_util::future::join_all(probe_futs).await;
        checks.extend(results);
    }

    DoctorReport { checks }
}

/// Append the same read-only macOS platform probes the bare
/// `dormantctl doctor` runs (see `crate::probe_all_offline`) to the live
/// daemon-backed doctor's checks — idle-clock health, display-sleep API
/// availability + current per-display state, active power assertions, and
/// input-filter readiness (CGEventTap / Accessibility).
/// All are read-only diagnostics; none of them ever blanks or wakes a
/// display — that stays exclusively under the `Exercise` control path.
/// Extracted out of [`run_inner`] to keep that function under
/// `clippy::too_many_lines`.
#[cfg(target_os = "macos")]
async fn push_macos_platform_checks(checks: &mut Vec<Check>, ignore_devices: &[String]) {
    let mut push = |mut check: Check| {
        check.category = Some("platform".into());
        checks.push(check);
    };
    push(probe_result_to_check(
        &crate::probes::macos_idle::probe_macos_idle().await,
    ));
    push(probe_result_to_check(
        &crate::probes::macos_display_sleep::probe_macos_display_sleep().await,
    ));
    push(probe_result_to_check(
        &crate::probes::macos_power::probe_macos_power().await,
    ));
    // Input-filter readiness: only report when ignore_devices is non-empty
    // (the feature is inactive otherwise — matching the Linux evdev probe's
    // Skip semantics).
    if !ignore_devices.is_empty() {
        push(probe_result_to_check(
            &crate::probes::input_filter::probe_input_filter(Some(ignore_devices)),
        ));
    }
}

/// Fetch a snapshot from the engine (bounded).  Returns an empty snapshot
/// on timeout / channel failure so the doctor still produces a report
/// rather than hanging the IPC connection.
async fn fetch_snapshot(ctl_tx: &mpsc::Sender<ControlMsg>) -> StateSnapshot {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if ctl_tx.send(ControlMsg::Snapshot(tx)).await.is_err() {
        tracing::warn!(
            event = "doctor_snapshot_unavailable",
            "engine not available"
        );
        return empty_snapshot();
    }
    match tokio::time::timeout(SNAPSHOT_FETCH_TIMEOUT, rx).await {
        Ok(Ok(snap)) => snap,
        Ok(Err(_canceled)) => empty_snapshot(),
        Err(_elapsed) => {
            tracing::warn!(
                event = "doctor_snapshot_timeout",
                "engine snapshot fetch timed out"
            );
            empty_snapshot()
        }
    }
}

fn empty_snapshot() -> StateSnapshot {
    StateSnapshot {
        sensors: vec![],
        zones: vec![],
        displays: vec![],
        pending_reload: None,
        rollback: None,
        kvm: None,
        wear_sampling_status: None,
    }
}

fn sensor_state_str(state: SensorState) -> &'static str {
    match state {
        SensorState::Present => "present",
        SensorState::Absent => "absent",
        SensorState::Unavailable => "unavailable",
    }
}

fn probe_result_to_check(res: &ProbeResult) -> Check {
    let status = match res.status {
        ProbeStatus::Pass => CheckStatus::Ok,
        ProbeStatus::Fail => CheckStatus::Fail,
        ProbeStatus::Skip => CheckStatus::Skip,
        ProbeStatus::NotSupported => CheckStatus::NotSupported,
    };
    let detail = if res.detail.is_empty() {
        None
    } else {
        Some(res.detail.clone())
    };
    Check {
        name: res.name.clone(),
        status,
        detail,
        category: res.category.clone(),
        subject: res.subject.clone(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use dormant_core::config::schema::{
        Config, Credentials, DaemonConfig, MqttSensorCfg, SensorConfig, SensorKind, UsbLd2410Cfg,
    };
    use dormant_core::rules::{DisplaySnapshot, SensorSnapshot, StateSnapshot};
    use indexmap::IndexMap;

    /// Build a minimal config with one USB + one MQTT sensor.
    fn test_config() -> Arc<Config> {
        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "front_desk".into(),
            SensorConfig::UsbLd2410(UsbLd2410Cfg {
                port: "/dev/ttyUSB0".into(),
                baud: 256_000,
                kind: SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
            }),
        );
        sensors.insert(
            "kitchen_motion".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://127.0.0.1:1".into(), // unreachable — will time out
                topic: "dormant/kitchen".into(),
                field: "/occupancy".into(),
                payload_on: None,
                payload_off: None,
                kind: SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors,
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        })
    }

    fn test_creds() -> Arc<Credentials> {
        Arc::new(Credentials::default())
    }

    /// Build a fake engine that responds to `ControlMsg::Snapshot` with the
    /// given snapshot and counts how many times the snapshot was requested.
    /// The optional `gate` oneshot delays the response so concurrent
    /// callers overlap inside `DoctorService::run`.
    fn spawn_fake_engine(
        snapshot: StateSnapshot,
        gate: Option<tokio::sync::oneshot::Sender<()>>,
        counter: Arc<AtomicUsize>,
    ) -> mpsc::Sender<ControlMsg> {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
        // Wrap the gate in `OnceLock` so we can consume it on the first
        // snapshot (the only one we want to delay); subsequent snapshots
        // return immediately.
        let mut gate_cell: std::sync::OnceLock<tokio::sync::oneshot::Sender<()>> =
            gate.map(std::sync::OnceLock::from).unwrap_or_default();
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::Snapshot(tx) = msg {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if let Some(g) = gate_cell.take() {
                        let _ = g.send(());
                        // Wait a tick so concurrent callers pile up on the
                        // coalesce lock before the response is delivered.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let _ = tx.send(snapshot.clone());
                }
            }
        });
        ctl_tx
    }

    fn owned_usb_snapshot() -> StateSnapshot {
        StateSnapshot {
            sensors: vec![SensorSnapshot {
                id: "front_desk".into(),
                state: SensorState::Present,
                last_seen_secs_ago: 3,
                reported: true,
            }],
            zones: vec![],
            displays: vec![(
                "main".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 1,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        }
    }

    /// Owned USB sensor → Skip, detail mentions "owned by daemon".  The
    /// USB probe code path is NEVER entered (we never call it from the
    /// service).  The MQTT probe DOES get spawned; it times out against
    /// the unreachable port.  This test confirms the owned-USB check is
    /// marked Skip and the rest of the report builds without panic.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_does_not_reopen_owned_usb() {
        let cfg = test_config();
        let creds = test_creds();
        let counter = Arc::new(AtomicUsize::new(0));
        let ctl_tx = spawn_fake_engine(owned_usb_snapshot(), None, counter.clone());

        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let service = DoctorService::new(ctl_tx, config_rx, creds_rx);
        let report = service.run().await;

        // Snapshot was fetched exactly once.
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // The USB check is Skip + "owned by daemon".
        let usb = report
            .checks
            .iter()
            .find(|c| c.name.contains("usb /dev/ttyUSB0"))
            .expect("usb check present");
        assert_eq!(usb.status, CheckStatus::Skip);
        let detail = usb.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("owned by daemon"),
            "USB detail should mark as owned: {detail}"
        );
        assert!(
            detail.contains("present"),
            "USB detail should include snapshot state: {detail}"
        );
    }

    /// Two concurrent `run()` calls share the ONE in-flight run: the
    /// underlying snapshot is fetched exactly once and both callers
    /// receive the same `DoctorReport` (Arc pointer equality).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_coalesces_concurrent_runs() {
        let cfg = test_config();
        let creds = test_creds();
        let counter = Arc::new(AtomicUsize::new(0));
        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let ctl_tx = spawn_fake_engine(owned_usb_snapshot(), Some(gate_tx), counter.clone());

        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let service = DoctorService::new(ctl_tx, config_rx, creds_rx);

        // Spawn two concurrent runs.  Tokio's current_thread runtime
        // polls them cooperatively; the first acquires the coalesce
        // mutex, kicks off the run, drops the guard; the second then
        // sees the Weak and joins.
        let s1 = service.clone();
        let s2 = service.clone();
        let h1 = tokio::spawn(async move { s1.run().await });
        let h2 = tokio::spawn(async move { s2.run().await });

        // Yield so both tasks reach the lock.
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Release the gate; both tasks should now resolve.  Awaiting the
        // receiver is a no-op here (we only signal, never receive value),
        // so unwrap on the Result of receiving.
        let _ = tokio::time::timeout(Duration::from_millis(100), gate_rx).await;

        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();

        // The snapshot was fetched exactly once.
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "snapshot should be fetched exactly once for coalesced runs"
        );

        // Both callers see the same report (same checks).
        assert_eq!(r1.checks.len(), r2.checks.len());
        assert_eq!(r1.checks, r2.checks);
    }

    /// A run after a previous run completed starts a fresh run (the Weak
    /// is gone, the slot is empty, a new future is spawned).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_runs_sequentially_when_not_concurrent() {
        let cfg = test_config();
        let creds = test_creds();
        let counter = Arc::new(AtomicUsize::new(0));
        let ctl_tx = spawn_fake_engine(owned_usb_snapshot(), None, counter.clone());

        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let service = DoctorService::new(ctl_tx, config_rx, creds_rx);

        service.run().await;
        service.run().await;
        service.run().await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "non-overlapping runs should each fetch the snapshot"
        );
    }

    /// A config that has no MQTT/HA/USB sensors still returns a valid
    /// report (no panic on the probe fan-out).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_handles_empty_sensors() {
        let cfg = Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        });
        let creds = test_creds();
        let counter = Arc::new(AtomicUsize::new(0));
        let ctl_tx = spawn_fake_engine(owned_usb_snapshot(), None, counter.clone());

        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let service = DoctorService::new(ctl_tx, config_rx, creds_rx);
        let report = service.run().await;

        // No sensor checks; only the display Skip from the snapshot.
        assert!(report.checks.iter().all(|c| !c.name.starts_with("usb ")
            && !c.name.starts_with("mqtt ")
            && !c.name.starts_with("ha ")));
    }

    /// Snapshot fetch timeout falls back to an empty snapshot, the report
    /// still builds (it just has no owned-device info), no panic.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_survives_snapshot_timeout() {
        // Build a fake engine that NEVER responds to Snapshot.  Dropping
        // the ctl_tx would close the channel — we need a real one that
        // just sits there.  Use a single channel that we keep alive in
        // the test scope; the service will hit the timeout path.
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::Snapshot(tx) = msg {
                    // Never reply; let the service's own timeout elapse.
                    drop(tx);
                }
            }
        });

        let cfg = test_config();
        let creds = test_creds();
        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let service = DoctorService::new(ctl_tx, config_rx, creds_rx);
        // We expect the report to be built from the empty snapshot fallback
        // — and the MQTT probe to time out (port 1 is unreachable).  The
        // whole run must complete in well under the 5s default test
        // timeout.
        let report = tokio::time::timeout(Duration::from_secs(10), service.run())
            .await
            .expect("doctor run should not hang");
        // No USB checks (snapshot was empty → no sensor rows).
        assert!(report.checks.iter().all(|c| !c.name.starts_with("usb ")));
    }

    // ── #185 Task 24b cycle B — per-display wear-sampling checks ─────────
    //
    // The doctor MUST emit ONE wear-sampling check PER configured
    // sampling display, never collapsed to a single row.  The fixture
    // here uses TWO displays with deliberately-different redacted
    // states so a regression that emits two checks for the same
    // subject (or one collapsed check) fails visibly.

    fn two_display_snapshot() -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![
                (
                    "desk".into(),
                    DisplaySnapshot {
                        phase: "active".into(),
                        inhibited: false,
                        paused: false,
                        cmd_gen: 1,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                    },
                ),
                (
                    "tv".into(),
                    DisplaySnapshot {
                        phase: "active".into(),
                        inhibited: false,
                        paused: false,
                        cmd_gen: 1,
                        scope: dormant_core::config::DisplayScope::Private,
                        owned: true,
                        observed_input_code: None,
                        panel_state: None,
                        controllers: vec![],
                        wake_attempts: 0,
                        last_blank_failed: false,
                        stage: None,
                    },
                ),
            ],
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        }
    }

    fn two_display_config() -> Arc<Config> {
        let mut cfg = (*test_config()).clone();
        cfg.wear.active_sampling.enabled = true;
        cfg.wear.active_sampling.sampled_display = None;
        cfg.wear.active_sampling.sampled_displays = vec!["desk".into(), "tv".into()];
        Arc::new(cfg)
    }

    /// When the daemon supplies a per-display status map, the doctor
    /// MUST emit one wear-sampling check PER configured display.
    /// Pin both the count and the per-row subject so a single-row
    /// implementation cannot pass.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_emits_one_wear_check_per_display_under_multi_selection() {
        let cfg = two_display_config();
        let creds = test_creds();
        let counter = Arc::new(AtomicUsize::new(0));
        let ctl_tx = spawn_fake_engine(two_display_snapshot(), None, counter.clone());

        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let mut statuses: BTreeMap<String, WearSamplingStatus> = BTreeMap::new();
        statuses.insert(
            "desk".into(),
            WearSamplingStatus {
                state: dormant_core::wear::WearSamplingState::Streaming,
                last_capture_age_s: Some(2),
                uniform_reason: None,
                bound_display: Some("desk".into()),
                granted_at_epoch_s: None,
                source_gate: None,
            },
        );
        statuses.insert(
            "tv".into(),
            WearSamplingStatus {
                state: dormant_core::wear::WearSamplingState::NeedsConsent,
                last_capture_age_s: None,
                uniform_reason: None,
                bound_display: Some("tv".into()),
                granted_at_epoch_s: None,
                source_gate: None,
            },
        );
        let (statuses_tx, statuses_rx) = watch::channel(statuses);
        drop(statuses_tx);

        let service = DoctorService::new_with_sampler_statuses(
            ctl_tx,
            config_rx,
            creds_rx,
            None,
            Some(statuses_rx),
        );
        let report = service.run().await;

        let wear_checks: Vec<&Check> = report
            .checks
            .iter()
            .filter(|c| c.name == "wear-sampling")
            .collect();
        assert_eq!(
            wear_checks.len(),
            2,
            "must emit one wear-sampling check per configured display, got {} (details: {:?})",
            wear_checks.len(),
            wear_checks
                .iter()
                .map(|c| (c.subject.clone(), c.status, c.detail.clone()))
                .collect::<Vec<_>>()
        );
        let subjects: std::collections::BTreeSet<&str> = wear_checks
            .iter()
            .filter_map(|c| c.subject.as_deref())
            .collect();
        assert_eq!(
            subjects,
            ["desk", "tv"].into_iter().collect(),
            "every wear-sampling check must carry its own subject"
        );
        // Both checks tagged as platform category (the singular probe
        // would also tag — proves we are on the new path).
        assert!(
            wear_checks
                .iter()
                .all(|c| c.category.as_deref() == Some("platform"))
        );
    }

    /// Redaction MUST hold at the doctor report layer too — even
    /// though the probe is the source of truth, an extra
    /// safety-net test here proves nothing in the Check
    /// construction path leaks the secret fields.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_per_display_report_does_not_leak_consent_secrets() {
        let cfg = two_display_config();
        let creds = test_creds();
        let counter = Arc::new(AtomicUsize::new(0));
        let ctl_tx = spawn_fake_engine(two_display_snapshot(), None, counter.clone());

        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let mut statuses: BTreeMap<String, WearSamplingStatus> = BTreeMap::new();
        statuses.insert(
            "desk".into(),
            WearSamplingStatus {
                state: dormant_core::wear::WearSamplingState::NeedsConsent,
                last_capture_age_s: None,
                uniform_reason: Some("token=persistent-id-should-not-leak".into()),
                bound_display: Some("persistent-id-should-not-leak".into()),
                granted_at_epoch_s: None,
                source_gate: None,
            },
        );
        statuses.insert(
            "tv".into(),
            WearSamplingStatus {
                state: dormant_core::wear::WearSamplingState::NeedsConsent,
                last_capture_age_s: None,
                uniform_reason: Some("token=other-persistent-id".into()),
                bound_display: Some("other-persistent-id".into()),
                granted_at_epoch_s: None,
                source_gate: None,
            },
        );
        let (statuses_tx, statuses_rx) = watch::channel(statuses);
        drop(statuses_tx);

        let service = DoctorService::new_with_sampler_statuses(
            ctl_tx,
            config_rx,
            creds_rx,
            None,
            Some(statuses_rx),
        );
        let report = service.run().await;
        for check in &report.checks {
            if let Some(detail) = &check.detail {
                assert!(
                    !detail.contains("token="),
                    "check detail leaked a token assignment: {detail}"
                );
                assert!(
                    !detail.contains("persistent-id"),
                    "check detail leaked persistent-id: {detail}"
                );
            }
        }
    }

    /// When the daemon does NOT supply the per-display map, the
    /// doctor falls back to the singular probe — exactly one
    /// wear-sampling check.  This is the legacy / off-Linux path.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn doctor_falls_back_to_singular_probe_without_per_display_map() {
        let cfg = two_display_config();
        let creds = test_creds();
        let counter = Arc::new(AtomicUsize::new(0));
        let ctl_tx = spawn_fake_engine(two_display_snapshot(), None, counter.clone());

        let (config_tx, config_rx) = watch::channel(cfg.clone());
        let (creds_tx, creds_rx) = watch::channel(creds.clone());
        drop(config_tx);
        drop(creds_tx);

        let service = DoctorService::new(ctl_tx, config_rx, creds_rx);
        let report = service.run().await;
        let wear_checks: Vec<&Check> = report
            .checks
            .iter()
            .filter(|c| c.name == "wear-sampling")
            .collect();
        assert_eq!(
            wear_checks.len(),
            1,
            "singular-probe fallback must emit exactly one check"
        );
    }
}
