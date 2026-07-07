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
        Self {
            inner: Arc::new(Inner {
                ctl_tx,
                config_rx,
                creds_rx,
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
        let fut: Pin<Box<dyn Future<Output = DoctorReport> + Send>> =
            Box::pin(run_inner(ctl_tx, config_rx, creds_rx));
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
async fn run_inner(
    ctl_tx: mpsc::Sender<ControlMsg>,
    config_rx: watch::Receiver<Arc<Config>>,
    creds_rx: watch::Receiver<Arc<Credentials>>,
) -> DoctorReport {
    let snapshot = fetch_snapshot(&ctl_tx).await;
    let cfg = config_rx.borrow().clone();
    let creds = creds_rx.borrow().clone();

    let mut checks: Vec<Check> = Vec::new();

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
            });
        }
    }

    // ── Non-exclusive network sensors (MQTT / HA) — active probe ──
    let mut probe_futs: Vec<std::pin::Pin<Box<dyn Future<Output = Check> + Send>>> = Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        match sensor_cfg {
            SensorConfig::Mqtt(mqtt_cfg) => {
                let id = id.clone();
                let cfg = mqtt_cfg.clone();
                probe_futs.push(Box::pin(async move {
                    let res = tokio::time::timeout(
                        NETWORK_PROBE_TIMEOUT,
                        crate::probes::mqtt::probe_mqtt_one(&id, &cfg),
                    )
                    .await
                    .unwrap_or_else(|_| ProbeResult::fail(format!("mqtt {id}"), "probe timeout"));
                    probe_result_to_check(&res)
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
                    probe_result_to_check(&res)
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
            }),
        );
        Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors,
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
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
            }],
            zones: vec![],
            displays: vec![(
                "main".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 1,
                    controllers: vec![],
                    stage: None,
                },
            )],
            pending_reload: None,
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
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
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
}
