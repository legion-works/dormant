//! End-to-end tests for the async rules engine (`dormant_core::rules`).
//!
//! These tests run under `#[tokio::test(start_paused = true)]` so that the
//! runtime's virtual clock advances only when no work is pending — letting us
//! drive minutes of simulated time in milliseconds of wall clock.
//!
//! Each test wires a real `RulesEngine` against:
//! - a `FakeSensorSource` driving the sensor event channel
//! - `RecordingSink` instances capturing every blank/wake the engine issues
//! - a `ZoneEngine` with one Any-mode zone (the fail-safe default policy keeps
//!   `Unavailable` events from blanking anything)
//!
//! Failures are loud (assertions), timings are virtual (no real-clock waits).

#![cfg(feature = "test-fakes")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use dormant_core::config::SensorKind;
use dormant_core::fakes::{FakeSensorSource, RecordingSink, SinkCmd};
use dormant_core::rules::{
    ControlMsg, DaemonEvent, DisplayRuntimeCfg, RuleRuntimeCfg, RulesEngine, RulesEngineConfig,
    SensorRuntimeCfg, StateSnapshot,
};
use dormant_core::state_machine::SmTimings;
use dormant_core::traits::{CommandSink, SensorSource};
use dormant_core::types::{
    BlankMode, CmdFailure, DisplayId, LadderStage, PresenceEvent, RuleId, SensorId, SensorState,
    StageKind, Timestamp, ZoneId,
};
use dormant_core::zone::{FusionMode, UnavailablePolicy, ZoneEngine, ZoneMember, ZoneSpec};

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Build a one-sensor one-zone `ZoneEngine` with the default fail-safe policy
/// (Unavailable → Present) — most tests assume this.
fn zone_with_sensor(sensor_id: &str, zone_id: &str) -> ZoneEngine {
    let sid = SensorId(sensor_id.into());
    let zid = ZoneId(zone_id.into());
    ZoneEngine::new(
        vec![ZoneSpec {
            id: zid,
            mode: FusionMode::Any,
            members: vec![ZoneMember::Sensor(sid.clone())],
            weights: HashMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        }],
        &[sid],
    )
    .expect("zone spec is well-formed")
}

/// Default timings for most tests: 60s grace, no startup holdoff, no dwell.
fn timings_grace_60s() -> SmTimings {
    SmTimings {
        grace_period: Duration::from_secs(60),
        min_blank_time: Duration::from_secs(0),
        min_wake_time: Duration::from_secs(0),
        startup_holdoff: Duration::from_secs(0),
        wake_retry_interval: Duration::from_secs(60),
    }
}

fn display_cfg(id: &str) -> DisplayRuntimeCfg {
    DisplayRuntimeCfg {
        display: DisplayId(id.into()),
        blank_mode: BlankMode::PowerOff,
        ladder: vec![LadderStage {
            kind: StageKind::Controller(BlankMode::PowerOff),
            dwell: None,
        }],
        timings: timings_grace_60s(),
    }
}

fn rule_cfg(id: &str, zone: &str, displays: &[&str]) -> RuleRuntimeCfg {
    RuleRuntimeCfg {
        rule: RuleId(id.into()),
        zone: ZoneId(zone.into()),
        displays: displays.iter().map(|s| DisplayId((*s).into())).collect(),
    }
}

fn sensor_cfg(
    id: &str,
    kind: SensorKind,
    hold: Option<Duration>,
    stale: Duration,
) -> SensorRuntimeCfg {
    SensorRuntimeCfg {
        sensor: SensorId(id.into()),
        kind,
        hold_time: hold,
        stale_timeout: stale,
    }
}

/// Spawn the engine and a fake sensor source; return handles for the test to
/// drive the scenario and assert.
struct Harness {
    /// Kept alive so the source-side sender isn't dropped mid-scenario.
    /// Some tests (e.g. `pause_does_not_lose_zone_edges` Part B) also push
    /// events into the channel directly to drive specific edges.
    events_tx: mpsc::Sender<PresenceEvent>,
    ctl_tx: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
    engine_handle: tokio::task::JoinHandle<()>,
    source_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn spawn_engine(
    cfg: RulesEngineConfig,
    zones: ZoneEngine,
    executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
    script: Vec<(Duration, PresenceEvent)>,
) -> Harness {
    let (events_tx, events_rx) = mpsc::channel(64);
    let (ctl_tx, ctl_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();

    let engine = RulesEngine::new(
        cfg,
        zones,
        executors,
        HashMap::new(),
        Arc::new(dormant_core::ownership::AlwaysOwned),
    )
    .expect("valid engine config");
    let engine_cancel = cancel.clone();
    let engine_handle = tokio::spawn(async move {
        engine.run(events_rx, ctl_rx, engine_cancel).await;
    });

    let source = FakeSensorSource {
        id: "fake".into(),
        script,
    };
    let source_tx = events_tx.clone();
    let source_cancel = cancel.clone();
    let source_handle =
        tokio::spawn(async move { Box::new(source).run(source_tx, source_cancel).await });

    Harness {
        events_tx,
        ctl_tx,
        cancel,
        engine_handle,
        source_handle,
    }
}

async fn request_snapshot(ctl: &mpsc::Sender<ControlMsg>) -> StateSnapshot {
    let (tx, rx) = oneshot::channel();
    ctl.send(ControlMsg::Snapshot(tx))
        .await
        .expect("ctl channel open");
    rx.await
        .expect("snapshot reply (sender must not be dropped)")
}

// ── 1: clear-grace-blank-then-instant-wake ─────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn clear_grace_blank_then_instant_wake() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Present@0, Absent@10s, Present@100s.
    let script = vec![
        (
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(10),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
        (
            Duration::from_secs(90),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
    ];

    let harness = spawn_engine(cfg, zones, execs, script);

    // Wait long enough for the entire script to play out (virtual time).
    tokio::time::sleep(Duration::from_secs(120)).await;

    // Give the engine a moment to flush the final wake + result.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    // Find the first Blank and the first Wake.
    let blank_at = log
        .iter()
        .find_map(|(t, c)| matches!(c, SinkCmd::Blank(_)).then_some(*t));
    let wake_at = log
        .iter()
        .find_map(|(t, c)| matches!(c, SinkCmd::Wake).then_some(*t));

    let blank_at = blank_at.expect("expected at least one Blank");
    let wake_at = wake_at.expect("expected at least one Wake");

    // Absent@10s + grace 60s → blank at ~70s (±1s virtual).
    assert!(
        blank_at.abs_diff(Duration::from_secs(70)) <= Duration::from_secs(1),
        "blank at {blank_at:?}, expected ~70s"
    );
    assert!(
        matches!(
            log.iter().find(|(t, _c)| *t == blank_at).unwrap().1,
            SinkCmd::Blank(BlankMode::PowerOff)
        ),
        "blank should be PowerOff"
    );
    // Present@100s → wake at ~100s.
    assert!(
        wake_at.abs_diff(Duration::from_secs(100)) <= Duration::from_secs(1),
        "wake at {wake_at:?}, expected ~100s"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── 2: broker-loss-never-blanks ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn broker_loss_never_blanks() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Script: only Unavailable events over 10 virtual minutes.
    let script = (0..10)
        .map(|i| {
            (
                Duration::from_secs(60 * i),
                PresenceEvent::new(sensor.clone(), SensorState::Unavailable, Timestamp::now()),
            )
        })
        .collect::<Vec<_>>();

    let harness = spawn_engine(cfg, zones, execs, script);
    tokio::time::sleep(Duration::from_secs(60 * 10 + 1)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    let blanks = log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert_eq!(
        blanks, 0,
        "broker loss with fail-safe policy must never blank: log={log:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── 3: stale-sensor-sweeper-marks-unavailable ──────────────────────────────────

#[tokio::test(start_paused = true)]
async fn stale_sensor_sweeper_marks_unavailable() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    // stale_timeout is the key knob; 500ms lets the sweeper fire on its first
    // 1s tick (sweep period = max(1s, stale/2) = 1s; first sweep at 1s sees
    // ~1s of real elapsed since Present@0).
    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_millis(500),
        )],
    };

    let script = vec![(
        Duration::from_secs(0),
        PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
    )];

    let harness = spawn_engine(cfg, zones, execs, script);

    // Wait for the stale sweep to fire (≥ 1s virtual, plus a beat).
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // No blanks ever.
    let blanks = sink
        .log()
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert_eq!(blanks, 0, "stale sensor must not blank: {:?}", sink.log());

    // Snapshot: sensor state must be Unavailable.
    let snap = request_snapshot(&harness.ctl_tx).await;
    let sensor_snap = snap
        .sensors
        .iter()
        .find(|s| s.id == "desk")
        .expect("sensor 'desk' in snapshot");
    assert_eq!(
        sensor_snap.state,
        SensorState::Unavailable,
        "stale sensor must be marked Unavailable by the sweeper"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── 4: pause_control_msg_blocks_blank_until_resume ───────────────────────────

#[tokio::test(start_paused = true)]
async fn pause_control_msg_blocks_blank_until_resume() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Absent@10s only — drives the display into Grace.
    let script = vec![(
        Duration::from_secs(10),
        PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
    )];

    let harness = spawn_engine(cfg, zones, execs, script);

    // Wait until virtual 30s, then send Pause (indefinite).
    tokio::time::sleep(Duration::from_secs(30)).await;
    harness
        .ctl_tx
        .send(ControlMsg::Pause {
            rule: None,
            until: None,
        })
        .await
        .expect("ctl open");

    // Advance well past the original grace expiry at 70s, into virtual 120s.
    tokio::time::sleep(Duration::from_secs(90)).await;
    assert!(
        sink.log().is_empty(),
        "no blank should fire while paused: {:?}",
        sink.log()
    );

    // Resume at virtual 120s. Remaining grace = 40s (70 - 30 freeze). Blank
    // should fire at virtual 160s.
    harness
        .ctl_tx
        .send(ControlMsg::Resume { rule: None })
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_secs(60)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    let blank_at = log
        .iter()
        .find_map(|(t, c)| matches!(c, SinkCmd::Blank(_)).then_some(*t));
    let blank_at = blank_at.expect("expected at least one Blank after resume");
    assert!(
        blank_at.abs_diff(Duration::from_secs(160)) <= Duration::from_secs(2),
        "blank at {blank_at:?}, expected ~160s"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── 5: motion_sensor_hold_time_stretches_pulse ────────────────────────────────

#[tokio::test(start_paused = true)]
async fn motion_sensor_hold_time_stretches_pulse() {
    let sensor = SensorId("couch".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("couch", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    // Motion sensor with hold_time = 30s.
    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "couch",
            SensorKind::Motion,
            Some(Duration::from_secs(30)),
            Duration::from_secs(60),
        )],
    };

    let script = vec![
        (
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(1),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
    ];

    let harness = spawn_engine(cfg, zones, execs, script);

    // Subscribe to events.
    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    // Wait until just before 30s — no ZoneChanged{present:false} yet.
    tokio::time::sleep(Duration::from_secs(29)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(ev) = events_rx.try_recv() {
        if let DaemonEvent::ZoneChanged { present: false, .. } = ev {
            panic!("motion hold must NOT release the zone before 30s");
        }
    }

    // Wait past 30s + grace (60s) → blank fires.
    tokio::time::sleep(Duration::from_secs(70)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let log = sink.log();
    let blank_at = log
        .iter()
        .find_map(|(t, c)| matches!(c, SinkCmd::Blank(_)).then_some(*t));
    let blank_at = blank_at.expect("expected blank after motion hold + grace");
    assert!(
        blank_at.abs_diff(Duration::from_secs(90)) <= Duration::from_secs(2),
        "blank at {blank_at:?}, expected ~90s"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── 6: wake_failure_retries_until_success ─────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn wake_failure_retries_until_success() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    sink.push_wake_result(Err(CmdFailure {
        controller: "mon".into(),
        error: "E_DISPLAY_IO: timeout".into(),
    }));
    sink.push_wake_result(Err(CmdFailure {
        controller: "mon".into(),
        error: "E_DISPLAY_IO: timeout".into(),
    }));
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    let script = vec![
        (
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
        (
            Duration::from_secs(120),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
    ];

    let harness = spawn_engine(cfg, zones, execs, script);

    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    tokio::time::sleep(Duration::from_secs(130)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let wake_count = sink
        .log()
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Wake))
        .count();
    assert!(
        wake_count >= 3,
        "expected ≥3 Wake calls (initial + 2 retries), got {wake_count}"
    );

    let mut retry_broadcasts = 0;
    while let Ok(ev) = events_rx.try_recv() {
        if matches!(ev, DaemonEvent::WakeRetry { .. }) {
            retry_broadcasts += 1;
        }
    }
    assert!(
        retry_broadcasts >= 1,
        "expected ≥1 WakeRetry broadcast, got {retry_broadcasts}"
    );

    let snap = request_snapshot(&harness.ctl_tx).await;
    let display_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("display in snapshot");
    assert_eq!(display_snap.1.phase, "active", "phase should be active");

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── 7: force_blank_and_snapshot ──────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn force_blank_and_snapshot() {
    let _sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    tokio::time::sleep(Duration::from_secs(5)).await;
    harness
        .ctl_tx
        .send(ControlMsg::ForceBlank(display.clone()))
        .await
        .expect("ctl open");

    tokio::time::sleep(Duration::from_millis(100)).await;
    let snap = request_snapshot(&harness.ctl_tx).await;
    let d_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("display");
    assert!(
        d_snap.1.phase == "blanking" || d_snap.1.phase == "blanked",
        "phase should be blanking or blanked, got {}",
        d_snap.1.phase
    );
    assert!(
        sink.log()
            .iter()
            .any(|(_, c)| matches!(c, SinkCmd::Blank(_))),
        "expected at least one Blank"
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    harness
        .ctl_tx
        .send(ControlMsg::ForceWake(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let snap = request_snapshot(&harness.ctl_tx).await;
    let d_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("display");
    assert_eq!(
        d_snap.1.phase, "active",
        "phase should be active after ForceWake"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

#[tokio::test(start_paused = true)]
async fn motion_sensor_hold_time_stretches_pulse_multiple() {
    let sensor = SensorId("couch".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("couch", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "couch",
            SensorKind::Motion,
            Some(Duration::from_secs(30)),
            Duration::from_secs(60),
        )],
    };

    let script = vec![
        (
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(1),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
        (
            Duration::from_secs(2),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(3),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
    ];

    let harness = spawn_engine(cfg, zones, execs, script);

    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    // Wait until 31s. The first timer was at 30s. It should NOT release the zone.
    tokio::time::sleep(Duration::from_secs(31)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(ev) = events_rx.try_recv() {
        if let DaemonEvent::ZoneChanged { present: false, .. } = ev {
            panic!(
                "motion hold must NOT release the zone before 33s (first timer fired at 30s and incorrectly released it!)"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pause_double_application_bug() {
    let sensor = SensorId("couch".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("couch", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "couch",
            SensorKind::Motion,
            None,
            Duration::from_secs(60),
        )],
    };

    let script = vec![
        (
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(10),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
    ];

    let harness = spawn_engine(cfg, zones, execs, script);

    // Pause at t=5s (while present)
    tokio::time::sleep(Duration::from_secs(5)).await;
    harness
        .ctl_tx
        .send(ControlMsg::Pause {
            rule: Some(RuleId("r1".into())),
            until: None,
        })
        .await
        .unwrap();

    // At t=10s, sensor becomes absent. But rule is paused, so fan_zone_change_to_displays skips it!
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Resume at t=20s
    harness
        .ctl_tx
        .send(ControlMsg::Resume {
            rule: Some(RuleId("r1".into())),
        })
        .await
        .unwrap();

    // Wait for grace period (60s) + 5s
    tokio::time::sleep(Duration::from_secs(65)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    let blank_at = log
        .iter()
        .find_map(|(t, c)| matches!(c, SinkCmd::Blank(_)).then_some(*t));

    assert!(
        blank_at.is_some(),
        "Display should have blanked after resume + grace period, but it didn't because the absent event was skipped!"
    );
}

// ── 8: hold_rearm_extends_stretch (regression for Must 1) ────────────────────

/// Regression for the re-arm / stale-timer bug: each `Present` pushes a new
/// `HoldExpiry` timer, but the wheel is a min-heap so the *first* timer
/// (from the original `Present@0`) fires before the re-armed one
/// (from `Present@20`). With the old code the stale timer would disarm and
/// replay the held `Absent` at t=30, releasing the zone 20 seconds early.
#[tokio::test(start_paused = true)]
async fn hold_rearm_extends_stretch() {
    let sensor = SensorId("couch".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("couch", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "couch",
            SensorKind::Motion,
            Some(Duration::from_secs(30)),
            Duration::from_secs(3600),
        )],
    };

    // Present@0  → arms hold to t=30, pending_absent cleared.
    // Absent@1   → swallowed, pending_absent=Some(Absent@1).
    // Present@20 → re-arms hold to t=50, clears pending_absent.
    // Absent@21  → swallowed, pending_absent=Some(Absent@21).
    let script = vec![
        (
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(1),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
        (
            Duration::from_secs(19),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(1),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
    ];

    let harness = spawn_engine(cfg, zones, execs, script);

    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    // Before t=50: drain any events, panic on a release. The stale t=30
    // timer is the failure mode this test guards against.
    tokio::time::sleep(Duration::from_secs(49)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(ev) = events_rx.try_recv() {
        if let DaemonEvent::ZoneChanged { present: false, .. } = ev {
            panic!("re-arm must extend the hold: stale t=30 timer released the zone early");
        }
    }

    // After t=50 + grace (60s) → blank fires.
    tokio::time::sleep(Duration::from_secs(70)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let log = sink.log();
    let blank_at = log
        .iter()
        .find_map(|(t, c)| matches!(c, SinkCmd::Blank(_)).then_some(*t));
    let blank_at = blank_at.expect("expected blank after re-arm-extended hold + grace");
    assert!(
        blank_at.abs_diff(Duration::from_secs(110)) <= Duration::from_secs(2),
        "blank at {blank_at:?}, expected ~110s (t=50 hold expiry + 60s grace)"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── 9: pause_does_not_lose_zone_edges (regression for Must 2) ────────────────

/// Regression for the paused-rule zone-edge drop: the state machine
/// already freezes the blank path on its overlay (and leaves wake
/// unaffected), so the engine must keep feeding it `ZonePresent` edges
/// while paused. This test exercises both halves of the symmetry:
///
/// Part A — pause → Absent during pause → Resume → blank after grace,
///          without any further sensor event (the held `Absent` edge is
///          what drives the blank).
/// Part B — blanked+paused → Present during pause → instant wake
///          (wake path is never gated by pause).
#[allow(clippy::too_many_lines)] // two-part scenario (Absent-during-pause + Blanked+paused-wake)
#[tokio::test(start_paused = true)]
async fn pause_does_not_lose_zone_edges() {
    // ── Part A: pause-then-absent → blank after resume ─────────────────────
    {
        let sensor = SensorId("desk".into());
        let display = DisplayId("mon".into());

        let zones = zone_with_sensor("desk", "office");
        let sink = Arc::new(RecordingSink::new());
        let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        execs.insert(display.clone(), sink.clone());

        let cfg = RulesEngineConfig {
            rules: vec![rule_cfg("r1", "office", &["mon"])],
            displays: vec![display_cfg("mon")],
            sensors: vec![sensor_cfg(
                "desk",
                SensorKind::Presence,
                None,
                Duration::from_secs(3600),
            )],
        };

        // Present@0 then Absent@10.
        let script = vec![
            (
                Duration::from_secs(0),
                PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
            ),
            (
                Duration::from_secs(10),
                PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
            ),
        ];

        let harness = spawn_engine(cfg, zones, execs, script);

        // Pause at t=5s.
        tokio::time::sleep(Duration::from_secs(5)).await;
        harness
            .ctl_tx
            .send(ControlMsg::Pause {
                rule: None,
                until: None,
            })
            .await
            .expect("ctl open");

        // Absent@10s arrives during the pause — engine must still step the
        // machine with ZonePresent(false), which freezes grace with
        // remaining 60s. Without the fix the rule is skipped and the
        // machine never sees the edge, so the post-resume grace has no
        // pending edge to drive.

        // Resume at t=80s. Grace is unfrozen with 60s remaining → expires
        // at t=140s.
        tokio::time::sleep(Duration::from_secs(75)).await;
        harness
            .ctl_tx
            .send(ControlMsg::Resume { rule: None })
            .await
            .expect("ctl open");

        // Past t=140s → blank.
        tokio::time::sleep(Duration::from_secs(70)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let log = sink.log();
        let blank_at = log
            .iter()
            .find_map(|(t, c)| matches!(c, SinkCmd::Blank(_)).then_some(*t));
        let blank_at = blank_at.expect(
            "blank must fire after resume (the held Absent edge drove the grace countdown)",
        );
        assert!(
            blank_at.abs_diff(Duration::from_secs(140)) <= Duration::from_secs(2),
            "blank at {blank_at:?}, expected ~140s (resume + remaining grace)"
        );

        harness.cancel.cancel();
        let _ = harness.engine_handle.await;
        let _ = harness.source_handle.await;
    }

    // ── Part B: blanked+paused → Present during pause → instant wake ────
    {
        let sensor = SensorId("desk".into());
        let display = DisplayId("mon".into());

        let zones = zone_with_sensor("desk", "office");
        let sink = Arc::new(RecordingSink::new());
        let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        execs.insert(display.clone(), sink.clone());

        let cfg = RulesEngineConfig {
            rules: vec![rule_cfg("r1", "office", &["mon"])],
            displays: vec![display_cfg("mon")],
            sensors: vec![sensor_cfg(
                "desk",
                SensorKind::Presence,
                None,
                Duration::from_secs(3600),
            )],
        };

        // Absent@0s, then Present after the blank has fired.
        let script = vec![(
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        )];

        let harness = spawn_engine(cfg, zones, execs, script);

        // Wait for blank to fire (grace 60s, blank at ~70s).
        tokio::time::sleep(Duration::from_secs(80)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            sink.log()
                .iter()
                .any(|(_, c)| matches!(c, SinkCmd::Blank(_))),
            "blank should have fired before pause"
        );

        // Pause while blanked.
        harness
            .ctl_tx
            .send(ControlMsg::Pause {
                rule: None,
                until: None,
            })
            .await
            .expect("ctl open");

        // Send Present during the pause.
        harness
            .events_tx
            .send(PresenceEvent::new(
                sensor.clone(),
                SensorState::Present,
                Timestamp::now(),
            ))
            .await
            .expect("events open");

        // Wake should fire immediately — no grace wait, pause never gates
        // the wake path.
        tokio::time::sleep(Duration::from_secs(1)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let log = sink.log();
        let wake_at = log
            .iter()
            .rev()
            .find_map(|(t, c)| matches!(c, SinkCmd::Wake).then_some(*t));
        let wake_at = wake_at.expect("wake must fire instantly on Present during pause");
        // The first Wake is the 0s Absent's eventual wake… actually no,
        // Absent@0s → blank. The wake we expect is from the Present sent
        // during pause. There shouldn't be any earlier wake (display
        // started Active, absent → grace → blanked, no wake before now).
        assert!(
            wake_at.abs_diff(Duration::from_secs(81)) <= Duration::from_secs(2),
            "wake at {wake_at:?}, expected ~81s (pause + Present during pause)"
        );

        harness.cancel.cancel();
        let _ = harness.engine_handle.await;
        let _ = harness.source_handle.await;
    }
}

// ── SetInhibited: activity inhibitor freezes and releases grace ────────────────

#[tokio::test(start_paused = true)]
async fn set_inhibited_freezes_grace_until_released() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Present@0, Absent@10s → grace runs until ~70s.
    let script = vec![
        (
            Duration::from_secs(0),
            PresenceEvent::new(sensor.clone(), SensorState::Present, Timestamp::now()),
        ),
        (
            Duration::from_secs(10),
            PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
        ),
    ];

    let harness = spawn_engine(cfg, zones, execs, script);

    // Let the Absent land and grace start, then engage the inhibitor before
    // grace would expire.
    tokio::time::sleep(Duration::from_secs(30)).await;
    harness
        .ctl_tx
        .send(ControlMsg::SetInhibited {
            rule: None,
            inhibited: true,
        })
        .await
        .expect("ctl open");

    // Well past the original grace deadline — frozen grace must not blank.
    tokio::time::sleep(Duration::from_secs(90)).await;
    assert!(
        !sink
            .log()
            .iter()
            .any(|(_, c)| matches!(c, SinkCmd::Blank(_))),
        "inhibitor must freeze grace: no blank while inhibited"
    );

    // Release the inhibitor — grace resumes and the blank fires.
    harness
        .ctl_tx
        .send(ControlMsg::SetInhibited {
            rule: None,
            inhibited: false,
        })
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_secs(90)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        sink.log()
            .iter()
            .any(|(_, c)| matches!(c, SinkCmd::Blank(_))),
        "blank must fire once the inhibitor is released"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── SetPendingReload: runtime pending-reload flag ──────────────────────────────

#[tokio::test(start_paused = true)]
async fn set_pending_reload_surfaces_in_snapshot() {
    let display = DisplayId("mon".into());
    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display, sink);

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    assert!(
        request_snapshot(&harness.ctl_tx)
            .await
            .pending_reload
            .is_none()
    );

    harness
        .ctl_tx
        .send(ControlMsg::SetPendingReload(Some("bad edit".into())))
        .await
        .expect("ctl open");
    let snap = request_snapshot(&harness.ctl_tx).await;
    assert_eq!(snap.pending_reload.as_deref(), Some("bad edit"));

    harness
        .ctl_tx
        .send(ControlMsg::SetPendingReload(None))
        .await
        .expect("ctl open");
    assert!(
        request_snapshot(&harness.ctl_tx)
            .await
            .pending_reload
            .is_none()
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── Helper: spawn engine with a custom OwnershipGate ───────────────────────────

fn spawn_engine_with_gate(
    cfg: RulesEngineConfig,
    zones: ZoneEngine,
    executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
    script: Vec<(Duration, PresenceEvent)>,
    gate: Arc<dyn dormant_core::ownership::OwnershipGate>,
) -> Harness {
    let (events_tx, events_rx) = mpsc::channel(64);
    let (ctl_tx, ctl_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();

    let engine =
        RulesEngine::new(cfg, zones, executors, HashMap::new(), gate).expect("valid engine config");
    let engine_cancel = cancel.clone();
    let engine_handle = tokio::spawn(async move {
        engine.run(events_rx, ctl_rx, engine_cancel).await;
    });

    let source = FakeSensorSource {
        id: "fake".into(),
        script,
    };
    let source_tx = events_tx.clone();
    let source_cancel = cancel.clone();
    let source_handle =
        tokio::spawn(async move { Box::new(source).run(source_tx, source_cancel).await });

    Harness {
        events_tx,
        ctl_tx,
        cancel,
        engine_handle,
        source_handle,
    }
}

// ── Ownership gate: NeverOwns test double ────────────────────────────────────

struct NeverOwns;

impl dormant_core::ownership::OwnershipGate for NeverOwns {
    fn owns(&self, _display: &DisplayId) -> bool {
        false
    }
}

// ── Test: NeverOwns blocks blanking ─────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn never_owns_blocks_blanking() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Absent@0 → should drive grace → expiry, but NeverOwns gates entry.
    let script = vec![(
        Duration::from_secs(0),
        PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
    )];

    let harness = spawn_engine_with_gate(cfg, zones, execs, script, Arc::new(NeverOwns));

    // Wait past the grace period (60s) + blank time + settling.
    tokio::time::sleep(Duration::from_secs(120)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    // NeverOwns must prevent any blank — the ownership gate says "not
    // owned", so the machine's `owned` flag is false, which blocks
    // entry at the grace-expiry gate (`!self.owned` check).
    let blank_count = log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert_eq!(
        blank_count, 0,
        "NeverOwns must block blanking — expected 0 blanks, got {blank_count}: {log:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── Test: AlwaysOwned lets it blank (control) ────────────────────────────────

#[tokio::test(start_paused = true)]
async fn always_owned_lets_it_blank() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Absent@0 → grace expires → blank (AlwaysOwned always permits).
    let script = vec![(
        Duration::from_secs(0),
        PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
    )];

    let harness = spawn_engine_with_gate(
        cfg,
        zones,
        execs,
        script,
        Arc::new(dormant_core::ownership::AlwaysOwned),
    );

    // Wait past grace period + blank time.
    tokio::time::sleep(Duration::from_secs(120)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    let blank_count = log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert!(
        blank_count >= 1,
        "AlwaysOwned must permit blanking — expected >=1 blank, got {blank_count}: {log:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}
