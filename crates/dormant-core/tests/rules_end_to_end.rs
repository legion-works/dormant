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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use dormant_core::config::SensorKind;
use dormant_core::fakes::{
    FakeSensorSource, RecordingRenderSink, RecordingSink, RenderCmd, SinkCmd,
};
use dormant_core::rules::{
    ControlMsg, DaemonEvent, DisplayRuntimeCfg, RuleRuntimeCfg, RulesEngine, RulesEngineConfig,
    SensorRuntimeCfg, StateSnapshot,
};
use dormant_core::state_machine::SmTimings;
use dormant_core::traits::{CommandSink, RenderSink, SensorSource};
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

fn spawn_engine_with_render(
    cfg: RulesEngineConfig,
    zones: ZoneEngine,
    executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
    render_sinks: HashMap<DisplayId, Arc<dyn RenderSink>>,
    script: Vec<(Duration, PresenceEvent)>,
) -> Harness {
    let (events_tx, events_rx) = mpsc::channel(64);
    let (ctl_tx, ctl_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();

    let engine = RulesEngine::new(
        cfg,
        zones,
        executors,
        render_sinks,
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

// ── exercise pause-resume is guaranteed engine-side ───────────────────────────
//
// MUST RED-first proof: pause + resume live on different sides today
// (pause = engine via `ControlMsg::Exercise`; resume = IPC via
// `exercise_response_with_resume`).  The IPC path ONLY runs on
// `Ok(Ok(report))`; the timeout / dropped-reply arms skip the resume and
// the rule stays paused indefinitely.  This test simulates the dropped
// reply by dropping the receiver immediately after sending the
// `Exercise` control message — the engine's spawned sequence must STILL
// resume the rule via the `ExerciseResume` internal result.
//
// The test asserts via a snapshot of the engine state: after the
// sequence completes and the ExerciseResume is processed, the rule
// must not be paused (`paused_rules` is the private set, so the
// snapshot's `paused` flag on the display is the observable side).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn exercise_handler_resumes_rule_even_when_reply_receiver_dropped() {
    use dormant_core::traits::{PanelState, PowerState};

    struct ScriptedSink {
        states: Arc<std::sync::Mutex<Vec<PanelState>>>,
        log: Arc<std::sync::Mutex<Vec<SinkCmd>>>,
    }
    #[async_trait::async_trait]
    impl CommandSink for ScriptedSink {
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            self.log
                .lock()
                .expect("log lock poisoned")
                .push(SinkCmd::Blank(BlankMode::PowerOff));
            Ok(())
        }
        async fn wake(&self) -> Result<(), CmdFailure> {
            self.log
                .lock()
                .expect("log lock poisoned")
                .push(SinkCmd::Wake);
            Ok(())
        }
        async fn wake_once(&self) -> Result<(), CmdFailure> {
            self.wake().await
        }
        fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
            Vec::new()
        }
        async fn read_state(&self) -> Option<PanelState> {
            self.states.lock().expect("states lock poisoned").pop()
        }
    }

    let display = DisplayId("mon".into());
    let zones = zone_with_sensor("desk", "office");

    let log = Arc::new(std::sync::Mutex::new(Vec::<SinkCmd>::new()));
    let states_script: Vec<PanelState> = vec![
        PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        }, // restore-read
        PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        }, // post-wake
        PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(0),
        }, // post-blank
        PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        }, // baseline
    ];
    let states = Arc::new(std::sync::Mutex::new(states_script));
    let sink_arc: Arc<dyn CommandSink> = Arc::new(ScriptedSink {
        states: Arc::clone(&states),
        log: Arc::clone(&log),
    });

    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink_arc);

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r_office", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    // Send Exercise — but DROP THE RECEIVER immediately.  This is the
    // exact failure mode that stranded rules in the v1 design (the
    // IPC-layer Resume forwarder never fired because `reply.send`
    // observed the dropped receiver and bailed).
    let (reply_tx, reply_rx) = oneshot::channel();
    drop(reply_rx); // simulate IPC caller gone before the sequence completes
    harness
        .ctl_tx
        .send(ControlMsg::Exercise {
            display: display.clone(),
            reply: reply_tx,
        })
        .await
        .expect("ctl channel open");

    // The engine must still process the ExerciseResume via the run
    // loop's results_rx arm.  Poll the snapshot until `paused` flips
    // back to false (rule resumed).  Bounded wait — if the resume never
    // fires the test will time out and fail.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snap = request_snapshot(&harness.ctl_tx).await;
        let mon = snap
            .displays
            .iter()
            .find(|(id, _)| id == "mon")
            .expect("display 'mon' in snapshot");
        if !mon.1.paused {
            break;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "rule stayed paused after exercise sequence despite \
             dropped IPC receiver — pause-resume leak"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Sanity: the sequence actually ran (the sink log is non-empty),
    // proving the spawned task completed end-to-end before the
    // resume fired.
    assert!(
        !log.lock().expect("log lock poisoned").is_empty(),
        "exercise sequence must have completed before the resume fired"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

/// Hung-read timeout test (SHOULD 2): a sink whose `read_state` never
/// returns must not block the spawned sequence indefinitely.  The
/// per-read budget wraps the call; the verdict surfaces `Unconfirmable`
/// and the sequence proceeds to the next phase.  Uses a deterministic
/// in-memory hang so the test runs fast — no real timers, no long
/// sleeps.  Polls the snapshot's `paused` flag to confirm the sequence
/// completed AND the engine-owned rule resume fired (the MUST-1 path).
#[tokio::test(flavor = "current_thread")]
async fn exercise_sequence_unconfirmable_when_read_state_hangs() {
    use dormant_core::rules::ExerciseVerdict;
    use dormant_core::traits::PanelState;

    /// Sink whose `read_state` never returns — the per-read budget is
    /// what SHOULD 2 guarantees must fire.
    struct HungSink;

    #[async_trait::async_trait]
    impl CommandSink for HungSink {
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            Ok(())
        }
        async fn wake(&self) -> Result<(), CmdFailure> {
            Ok(())
        }
        async fn wake_once(&self) -> Result<(), CmdFailure> {
            Ok(())
        }
        fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
            Vec::new()
        }
        async fn read_state(&self) -> Option<PanelState> {
            // Pending forever — never resolves.  The helper's
            // `tokio::time::timeout` wrap is the only thing that saves
            // the spawned task from blocking forever.
            std::future::pending::<Option<PanelState>>().await
        }
    }

    let display = DisplayId("mon".into());
    let zones = zone_with_sensor("desk", "office");
    let sink_arc: Arc<dyn CommandSink> = Arc::new(HungSink);
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink_arc);

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r_office", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    // Drop the receiver immediately so the IPC timeout / dropped-reply
    // arms fire (this exercises the ExerciseResume internal-result
    // path the MUST fix added) AND the hung read would otherwise leak
    // the spawned task — bounded_read_state's timeout is the only thing
    // that lets the sequence complete.
    let (reply_tx, reply_rx) = oneshot::channel();
    drop(reply_rx);
    harness
        .ctl_tx
        .send(ControlMsg::Exercise {
            display: display.clone(),
            reply: reply_tx,
        })
        .await
        .expect("ctl channel open");

    // Poll the snapshot: the rule must resume (engine-owned resume
    // fired via ExerciseResume internal result), AND the sequence
    // must complete (proving the hung reads didn't block the task —
    // bounded_read_state returned None for every site, every step is
    // Unconfirmable).
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let snap = request_snapshot(&harness.ctl_tx).await;
        let mon = snap
            .displays
            .iter()
            .find(|(id, _)| id == "mon")
            .expect("display 'mon' in snapshot");
        if !mon.1.paused {
            break;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "exercise sequence with hung read_state never completed \
             and rule stayed paused — per-read budget leak"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Reference ExerciseVerdict so the import stays in scope if a
    // future reader adds an additional assertion on the report shape.
    let _ = ExerciseVerdict::Unconfirmable;

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

// ── F12: retained-vacant worst-case timeline pin (spec §7#2, §8) ──────────────

/// PIN test, not RED-first in the usual sense (P1 pinned exception): this
/// test pins PRE-EXISTING engine behavior — the blank-at-grace-expiry
/// timeline for a single-sensor zone whose FIRST-EVER event is `Absent` at
/// t≈0 (the shape of a broker-retained "vacant" MQTT message delivered on
/// subscribe, which bypasses `unavailable_policy` entirely and is
/// indistinguishable to the engine from a live Absent edge). That timeline
/// was already GREEN before Task 3 — every assertion here except the LAST
/// one is a pin, not new coverage. Only the trailing `reported` assertion
/// is genuinely new: it was RED until the `RulesEngine.reported` field and
/// its `handle_presence_event`/`send_snapshot` wiring landed in this task.
/// A future reviewer must not misfile the whole test as a TDD violation —
/// the timeline is a documented, ACCEPTED residual risk (§7#2), not a bug
/// this test is meant to catch; it exists so the exposure is executable
/// truth, per the F12 finding.
#[tokio::test(start_paused = true)]
async fn retained_vacant_worst_case_timeline() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    // Default timings per §7#2: grace_period = 60s, startup_holdoff = 30s.
    // holdoff gates the FIRST blank only (state_machine.rs) and is already
    // elapsed (30s < 60s) by the time grace expires — it does not add any
    // extra delay here, which is the crux of the accepted exposure.
    let timings = SmTimings {
        grace_period: Duration::from_secs(60),
        min_blank_time: Duration::from_secs(0),
        min_wake_time: Duration::from_secs(0),
        startup_holdoff: Duration::from_secs(30),
        wake_retry_interval: Duration::from_secs(60),
    };
    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![DisplayRuntimeCfg {
            display: display.clone(),
            blank_mode: BlankMode::PowerOff,
            ladder: vec![LadderStage {
                kind: StageKind::Controller(BlankMode::PowerOff),
                dwell: None,
            }],
            timings,
        }],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // The retained-vacant shape: Absent is the VERY FIRST event this sensor
    // ever delivers, at t≈0 — no prior Present was ever observed (a
    // motionless occupant + a stale retained "vacant" publish).
    let script = vec![(
        Duration::from_secs(0),
        PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
    )];

    let harness = spawn_engine(cfg, zones, execs, script);

    // Advance to grace expiry (~60s virtual) plus a beat for dispatch to
    // land.
    tokio::time::sleep(Duration::from_secs(65)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    let blank_at = log
        .iter()
        .find_map(|(t, c)| matches!(c, SinkCmd::Blank(_)).then_some(*t));
    let blank_at = blank_at.expect(
        "retained-vacant worst case: blank must fire at grace expiry \
         (§7#2 accepted exposure) — PIN, not new behavior",
    );
    assert!(
        blank_at.abs_diff(Duration::from_secs(60)) <= Duration::from_secs(2),
        "blank at {blank_at:?}, expected ~60s (grace expiry; the 30s \
         startup_holdoff was already elapsed and added no extra delay)"
    );

    // NEW assertion (RED pre-T3): the snapshot must show the sensor as
    // `reported == true` — the worst-case Absent IS a real report from the
    // device (as distinct from a sensor that has never reported at all).
    // `reported` is what lets an operator tell "never heard from" apart
    // from "went away" on a cold-start snapshot.
    let snap = request_snapshot(&harness.ctl_tx).await;
    let sensor_snap = snap
        .sensors
        .iter()
        .find(|s| s.id == "desk")
        .expect("sensor 'desk' in snapshot");
    assert!(
        sensor_snap.reported,
        "sensor must show reported == true after grace expiry — it HAS \
         delivered an event (the worst-case Absent), even though that \
         event looks like a vacancy (persistence across state flips is \
         covered separately by `reported_false_until_first_event_then_true`)"
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

// ── Ownership gate: FlipGate test double (dynamic, edge-sensitive) ───────────

/// Gate backed by shared mutable state — starts `true`, flips externally.
/// Exercises the run-loop `feed_ownership` path (the constructor seed sets
/// `owned=true`, so only a mid-run flip can change the verdict).
struct FlipGate(Arc<AtomicBool>);

impl dormant_core::ownership::OwnershipGate for FlipGate {
    fn owns(&self, _display: &DisplayId) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

// ── Test: flip gate during grace blocks blank (run-loop feed) ─────────────

/// The gate starts `true` so the display enters Grace normally.  After
/// the absent edge is processed but BEFORE the grace timer fires, the
/// gate is flipped to `false`.  This proves the run-loop `feed_ownership`
/// call in `fire_due_timers` actually consults the gate and feeds
/// `OwnershipChanged(false)` — the constructor seed can't catch a
/// post-construction flip.
#[tokio::test(start_paused = true)]
async fn flip_gate_during_grace_blocks_blank() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let flag = Arc::new(AtomicBool::new(true));

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

    // Absent@0 → enters Grace (owned=seed true → proceeds).
    let script = vec![(
        Duration::from_secs(0),
        PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
    )];

    let harness = spawn_engine_with_gate(
        cfg,
        zones,
        execs,
        script,
        Arc::new(FlipGate(Arc::clone(&flag))),
    );

    // Let the absent edge land and grace start (t=0 → t=30s).
    tokio::time::sleep(Duration::from_secs(30)).await;

    // Flip the gate BEFORE the grace timer fires at ~60s.
    flag.store(false, Ordering::Relaxed);

    // Advance past the grace deadline (60s) + settling.
    tokio::time::sleep(Duration::from_secs(90)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    let blank_count = log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert_eq!(
        blank_count, 0,
        "gate flip during grace must block blank — expected 0 blanks, got {blank_count}: {log:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── Test: control — gate stays true, blank fires (paired control) ────────

/// Same scenario as `flip_gate_during_grace_blocks_blank` but without
/// the flip.  The gate stays `true` → the grace timer fires → the
/// machine blanks normally.  This isolates the flip as the cause.
#[tokio::test(start_paused = true)]
async fn flip_gate_control_blank_fires_when_owned() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let sink = Arc::new(RecordingSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let flag = Arc::new(AtomicBool::new(true));

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

    let script = vec![(
        Duration::from_secs(0),
        PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
    )];

    let harness = spawn_engine_with_gate(
        cfg,
        zones,
        execs,
        script,
        Arc::new(FlipGate(Arc::clone(&flag))),
    );

    // Advance past the grace deadline (60s) + settling.  Gate stays
    // `true` the whole time → blank fires normally.
    tokio::time::sleep(Duration::from_secs(120)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = sink.log();
    let blank_count = log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert!(
        blank_count >= 1,
        "paired control must blank — expected >=1 blank, got {blank_count}: {log:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── Render ladder: engine drives RenderSink (closes T3 S3 gap) ─────────────────

/// A display whose ladder starts with `RenderBlack` must call
/// `RenderSink::show` when the zone goes absent, and
/// `RenderSink::teardown` when presence returns — no
/// controller-level blank/wake should fire for the render stage.
#[allow(clippy::too_many_lines)]
#[tokio::test(start_paused = true)]
async fn render_ladder_drives_render_sink() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let cmd_sink = Arc::new(RecordingSink::new());
    let render_sink = Arc::new(RecordingRenderSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), cmd_sink.clone());
    let mut renders: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
    renders.insert(display.clone(), render_sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![DisplayRuntimeCfg {
            display: display.clone(),
            blank_mode: BlankMode::PowerOff,
            ladder: vec![
                LadderStage {
                    kind: StageKind::RenderBlack,
                    dwell: None, // terminal — stays until presence returns
                },
                LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: None,
                },
            ],
            timings: timings_grace_60s(),
        }],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Absent@10s → grace 60s → show triggered at ~70s.
    // Present@100s → teardown + active.
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

    let harness = spawn_engine_with_render(cfg, zones, execs, renders, script);

    // Wait past the full sequence + flush spawned tasks.
    tokio::time::sleep(Duration::from_secs(120)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let render_log = render_sink.log();

    // Must have recorded a Show(RenderBlack).
    let show_call = render_log.iter().find(|(_, c)| {
        matches!(
            c,
            RenderCmd::Show {
                kind: StageKind::RenderBlack,
                ..
            }
        )
    });
    assert!(
        show_call.is_some(),
        "expected RenderSink::show(RenderBlack), got {render_log:?}"
    );
    let (show_at, _show_cmd) = show_call.unwrap();
    assert!(
        show_at.abs_diff(Duration::from_secs(70)) <= Duration::from_secs(2),
        "show at {show_at:?}, expected ~70s (absent@10 + grace 60s)"
    );

    // Must have recorded a Teardown.
    let teardown_call = render_log
        .iter()
        .find(|(_, c)| matches!(c, RenderCmd::Teardown { .. }));
    assert!(
        teardown_call.is_some(),
        "expected RenderSink::teardown, got {render_log:?}"
    );
    let (td_at, _td_cmd) = teardown_call.unwrap();
    assert!(
        td_at.abs_diff(Duration::from_secs(100)) <= Duration::from_secs(2),
        "teardown at {td_at:?}, expected ~100s (present@100)"
    );

    // No controller-level blank or wake for the render stage.
    let cmd_log = cmd_sink.log();
    let blanks = cmd_log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    let wakes = cmd_log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Wake))
        .count();
    assert_eq!(
        blanks, 0,
        "no controller blank should fire for a render stage: {cmd_log:?}"
    );
    assert_eq!(
        wakes, 0,
        "no controller wake should fire for a render stage: {cmd_log:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

/// When the `RenderSink` fails `show`, the engine must fall through
/// to the next ladder rung — here a controller `PowerOff` blank.
/// This proves the engine-level render fall-through (complements the
/// SM-level test).
#[tokio::test(start_paused = true)]
async fn render_failure_falls_through_to_controller() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let cmd_sink = Arc::new(RecordingSink::new());
    let render_sink = Arc::new(RecordingRenderSink::new());
    // Script the render sink to fail.
    render_sink.push_show_result(Err(CmdFailure {
        controller: "wayland".into(),
        error: "E_RENDER_UNAVAILABLE: compositor gone".into(),
    }));
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), cmd_sink.clone());
    let mut renders: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
    renders.insert(display.clone(), render_sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![DisplayRuntimeCfg {
            display: display.clone(),
            blank_mode: BlankMode::PowerOff,
            ladder: vec![
                LadderStage {
                    kind: StageKind::RenderBlack,
                    dwell: None,
                },
                LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: None,
                },
            ],
            timings: timings_grace_60s(),
        }],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    let script = vec![(
        Duration::from_secs(10),
        PresenceEvent::new(sensor.clone(), SensorState::Absent, Timestamp::now()),
    )];

    let harness = spawn_engine_with_render(cfg, zones, execs, renders, script);

    // Absent@10 + grace 60s + render-fail → fall-through → blank.
    tokio::time::sleep(Duration::from_secs(120)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Render sink must have recorded a Show attempt.
    let render_log = render_sink.log();
    assert!(
        render_log.iter().any(|(_, c)| matches!(
            c,
            RenderCmd::Show {
                kind: StageKind::RenderBlack,
                ..
            }
        )),
        "expected RenderSink::show to be called, got {render_log:?}"
    );

    // Controller sink must have recorded a Blank — the fall-through.
    let cmd_log = cmd_sink.log();
    let blanks = cmd_log
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert!(
        blanks >= 1,
        "render failure must fall through to controller Blank, got {cmd_log:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── Engine-level dwell-advance: StageTick driven by the real timer wheel ────────

/// A ladder `[render_black dwell=30s, power_off]` must escalate from the render
/// stage to the controller blank after the dwell elapses — and NOT before.
/// This is the first engine-level test that exercises the timer wheel's
/// `DisplayStageTick` path end-to-end (the `StageTick` → advance path has never
/// run through the real engine timer wheel before — every prior ladder test uses
/// `dwell: None`).
#[allow(clippy::too_many_lines)]
#[tokio::test(start_paused = true)]
async fn render_ladder_dwell_advances_to_controller_blank() {
    let sensor = SensorId("desk".into());
    let display = DisplayId("mon".into());

    let zones = zone_with_sensor("desk", "office");
    let cmd_sink = Arc::new(RecordingSink::new());
    let render_sink = Arc::new(RecordingRenderSink::new());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), cmd_sink.clone());
    let mut renders: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
    renders.insert(display.clone(), render_sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r1", "office", &["mon"])],
        displays: vec![DisplayRuntimeCfg {
            display: display.clone(),
            blank_mode: BlankMode::PowerOff,
            ladder: vec![
                LadderStage {
                    kind: StageKind::RenderBlack,
                    dwell: Some(Duration::from_secs(30)),
                },
                LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: None,
                },
            ],
            timings: timings_grace_60s(),
        }],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Absent@10s → grace 60s → ladder entry at ~70s.
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

    let harness = spawn_engine_with_render(cfg, zones, execs, renders, script);

    // Advance past the grace period (absent@10 + 60s grace = 70s) — the
    // render sink should be called with Show(RenderBlack).
    tokio::time::sleep(Duration::from_secs(72)).await;
    // Give the spawned render task a moment to complete and the engine to
    // process the RenderResult.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let render_log = render_sink.log();
    let show_call = render_log.iter().find(|(_, c)| {
        matches!(
            c,
            RenderCmd::Show {
                kind: StageKind::RenderBlack,
                ..
            }
        )
    });
    assert!(
        show_call.is_some(),
        "expected RenderSink::show(RenderBlack), got {render_log:?}"
    );

    // Before the dwell expires (70s + 30s = 100s), no controller blank
    // must have fired — the machine should be in the Staged phase.
    tokio::time::sleep(Duration::from_secs(25)).await; // now at ~97s
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cmd_log_before = cmd_sink.log();
    let blanks_before = cmd_log_before
        .iter()
        .filter(|(_, c)| matches!(c, SinkCmd::Blank(_)))
        .count();
    assert_eq!(
        blanks_before, 0,
        "no controller blank must fire before dwell elapses (log={cmd_log_before:?})"
    );

    // Advance past the dwell deadline (100s).
    tokio::time::sleep(Duration::from_secs(10)).await; // now at ~107s
    tokio::time::sleep(Duration::from_millis(200)).await;

    let cmd_log = cmd_sink.log();
    let blank = cmd_log
        .iter()
        .find(|(_, c)| matches!(c, SinkCmd::Blank(BlankMode::PowerOff)));
    assert!(
        blank.is_some(),
        "expected Blank(PowerOff) escalation after dwell, got {cmd_log:?}"
    );
    let (blank_at, _) = blank.unwrap();
    // Should fire at ~100s (70s entry + 30s dwell).
    assert!(
        blank_at.abs_diff(Duration::from_secs(100)) <= Duration::from_secs(5),
        "blank at {blank_at:?}, expected ~100s (entry@70 + dwell 30s)"
    );

    // The render surface that was up during the render stage must be torn
    // down BEFORE the controller blank fires — otherwise the orphaned
    // wl_surface is left mapped on top of the now-blanked panel and
    // blocks visibility.  The RecordingRenderSink captures every
    // teardown; we must see at least one.  Re-read the render log AFTER
    // the dwell escalation (~100s) — the earlier snapshot at ~72s only
    // saw the Show.
    let render_log_after = render_sink.log();
    let teardown_after_dwell = render_log_after
        .iter()
        .filter(|(_, c)| matches!(c, RenderCmd::Teardown { .. }))
        .count();
    assert!(
        teardown_after_dwell >= 1,
        "dwell escalation into the controller stage must tear down the render surface — got {render_log_after:?}"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── emergency-wake handler ────────────────────────────────────────────────────
//
// RED-first pin for `ControlMsg::EmergencyWake`: both halves of the contract
// — wake EVERY display directly, and pause EVERY rule indefinitely — must
// hold.  A version that only wakes (without pausing) or only pauses (without
// waking each display) fails this test.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn emergency_wake_handler_wakes_every_display_and_pauses_all_rules() {
    use dormant_core::rules::EmergencyWakeResult;

    let desk = DisplayId("desk".into());
    let wall = DisplayId("wall".into());

    // Two sensors, two zones, two rules → three displays total (the third
    // is manual-only, no rule).  All three must wake; all rules must pause.
    let desk_zone_sensor = SensorId("s_desk".into());
    let wall_zone_sensor = SensorId("s_wall".into());

    let zones = ZoneEngine::new(
        vec![
            ZoneSpec {
                id: ZoneId("desk_z".into()),
                mode: FusionMode::Any,
                members: vec![ZoneMember::Sensor(desk_zone_sensor.clone())],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            },
            ZoneSpec {
                id: ZoneId("wall_z".into()),
                mode: FusionMode::Any,
                members: vec![ZoneMember::Sensor(wall_zone_sensor.clone())],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            },
        ],
        &[desk_zone_sensor, wall_zone_sensor],
    )
    .expect("two zones is well-formed");

    let desk_sink = Arc::new(RecordingSink::new());
    let wall_sink = Arc::new(RecordingSink::new());
    let manual_sink = Arc::new(RecordingSink::new());

    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(desk.clone(), desk_sink.clone());
    execs.insert(wall.clone(), wall_sink.clone());
    execs.insert(DisplayId("manual".into()), manual_sink.clone());

    let cfg = RulesEngineConfig {
        rules: vec![
            rule_cfg("r_desk", "desk_z", &["desk"]),
            rule_cfg("r_wall", "wall_z", &["wall"]),
        ],
        displays: vec![
            DisplayRuntimeCfg {
                display: desk.clone(),
                blank_mode: BlankMode::PowerOff,
                ladder: vec![LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: None,
                }],
                timings: timings_grace_60s(),
            },
            DisplayRuntimeCfg {
                display: wall.clone(),
                blank_mode: BlankMode::PowerOff,
                ladder: vec![LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: None,
                }],
                timings: timings_grace_60s(),
            },
            DisplayRuntimeCfg {
                display: DisplayId("manual".into()),
                blank_mode: BlankMode::PowerOff,
                ladder: vec![LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: None,
                }],
                timings: timings_grace_60s(),
            },
        ],
        sensors: vec![
            sensor_cfg(
                "s_desk",
                SensorKind::Presence,
                None,
                Duration::from_secs(3600),
            ),
            sensor_cfg(
                "s_wall",
                SensorKind::Presence,
                None,
                Duration::from_secs(3600),
            ),
        ],
    };

    // Empty event script — engine stays quiet until we send the
    // emergency-wake control message.
    let harness = spawn_engine(cfg, zones, execs, vec![]);

    // Send EmergencyWake.
    let (tx, rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::EmergencyWake { reply: tx })
        .await
        .expect("ctl channel open");
    let report = rx.await.expect("reply not dropped");

    // Half 1: paused.
    assert!(report.paused, "report.paused must be true: {report:?}");

    // Half 2: every executor received a wake_once() call.  The
    // RecordingSink override calls self.wake(), so we look for any Wake
    // entry in each sink's log.
    for (name, sink) in [
        ("desk", &desk_sink),
        ("wall", &wall_sink),
        ("manual", &manual_sink),
    ] {
        let wakes = sink
            .log()
            .iter()
            .filter(|(_, c)| matches!(c, SinkCmd::Wake))
            .count();
        assert!(
            wakes >= 1,
            "emergency-wake must wake {name} (saw {wakes} wake calls in log)",
        );
    }

    // Each executor must be invoked EXACTLY once (wake_once, not the
    // retry-loop wake).  Multiple wake entries would mean the wrong
    // method was called by the engine.
    for (name, sink) in [
        ("desk", &desk_sink),
        ("wall", &wall_sink),
        ("manual", &manual_sink),
    ] {
        let wakes = sink
            .log()
            .iter()
            .filter(|(_, c)| matches!(c, SinkCmd::Wake))
            .count();
        assert_eq!(
            wakes, 1,
            "{name} executor must have been wake_once'd exactly once, got {wakes}",
        );
    }

    // Verify the per-display rows in the report — one row per display,
    // every row ok=true.
    let expected_displays: std::collections::HashSet<DisplayId> =
        [desk.clone(), wall.clone(), DisplayId("manual".into())]
            .into_iter()
            .collect();
    let report_displays: std::collections::HashSet<DisplayId> = report
        .displays
        .iter()
        .map(|r: &EmergencyWakeResult| r.display.clone())
        .collect();
    assert_eq!(
        expected_displays, report_displays,
        "report must contain one row per display the engine owns",
    );
    for row in &report.displays {
        assert!(row.ok, "every per-display wake should succeed: {row:?}");
        assert!(row.error.is_none(), "ok row should have no error: {row:?}");
    }

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}
// ── emergency-wake parallelism barrier pin ─────────────────────────────
//
// SERIAL implementation CANNOT pass this test, PARALLEL implementation
// always passes.  Uses a `tokio::sync::Barrier` so the test is fully
// deterministic — no `tokio::time::sleep` races, no wall-clock fuzziness.
//
// Mechanism:
// - N=3 fake sinks share a single `Arc<Barrier>` of size 3.
// - Each sink's `wake_once()` (and `wake()`) awaits the barrier before
//   returning `Ok(())`.
// - SERIAL handler: outer task awaits sink[0].wake_once() → blocks on
//   the barrier because no second task ever runs.  Outer never finishes,
//   the oneshot never sends, `rx.await` times out — assertion fails.
// - PARALLEL handler: outer task spawns 3 inner tasks up-front; all 3
//   hit the barrier together, it releases, all 3 succeed, the report
//   ships.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines, clippy::match_wild_err_arm)]
async fn emergency_wake_handler_dispatches_displays_concurrently() {
    /// Test-only `CommandSink` whose `wake_once` (and `wake`) block on
    /// a shared `Barrier` before returning `Ok(())`.  Records every
    /// call so the test can sanity-check that all three sinks reached
    /// the barrier.
    struct BarrierSink {
        barrier: Arc<tokio::sync::Barrier>,
        started: Arc<std::sync::Mutex<Vec<DisplayId>>>,
        id: DisplayId,
    }

    #[async_trait::async_trait]
    impl CommandSink for BarrierSink {
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            Ok(())
        }
        async fn wake(&self) -> Result<(), CmdFailure> {
            // Block here too so a serial caller that dispatches the
            // wrong method (wake, not wake_once) also deadlocks visibly.
            self.barrier.wait().await;
            self.started
                .lock()
                .expect("started lock poisoned")
                .push(self.id.clone());
            Ok(())
        }
        async fn wake_once(&self) -> Result<(), CmdFailure> {
            self.barrier.wait().await;
            self.started
                .lock()
                .expect("started lock poisoned")
                .push(self.id.clone());
            Ok(())
        }
        fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
            Vec::new()
        }
    }

    let n = 3;
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let started: Arc<std::sync::Mutex<Vec<DisplayId>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let displays: Vec<DisplayId> = (0..n).map(|i| DisplayId(format!("d{i}"))).collect();

    let zone_sensor = SensorId("s".into());
    let zones = ZoneEngine::new(
        vec![ZoneSpec {
            id: ZoneId("z".into()),
            mode: FusionMode::Any,
            members: vec![ZoneMember::Sensor(zone_sensor.clone())],
            weights: HashMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        }],
        &[zone_sensor],
    )
    .expect("one zone is well-formed");

    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    let mut display_cfgs: Vec<DisplayRuntimeCfg> = Vec::new();
    for d in &displays {
        let sink: Arc<dyn CommandSink> = Arc::new(BarrierSink {
            barrier: Arc::clone(&barrier),
            started: Arc::clone(&started),
            id: d.clone(),
        });
        execs.insert(d.clone(), sink);
        display_cfgs.push(DisplayRuntimeCfg {
            display: d.clone(),
            blank_mode: BlankMode::PowerOff,
            ladder: vec![LadderStage {
                kind: StageKind::Controller(BlankMode::PowerOff),
                dwell: None,
            }],
            timings: timings_grace_60s(),
        });
    }

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r", "z", &["d0", "d1", "d2"])],
        displays: display_cfgs,
        sensors: vec![sensor_cfg(
            "s",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    // Empty script — engine stays quiet until the EmergencyWake.
    let harness = spawn_engine(cfg, zones, execs, vec![]);

    // Drive the handler.
    let (tx, rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::EmergencyWake { reply: tx })
        .await
        .expect("ctl channel open");

    // Bounded wait for the report. 5 seconds is generous — under a
    // serial handler this elapses because the barrier deadlocks the
    // outer task.
    let report = match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_dropped)) => panic!("reply oneshot dropped before sending"),
        Err(_elapsed) => panic!(
            "emergency-wake reply did not arrive within 5s — handler is serial, \
                 not parallel (one slow wake_once is starving the others on the barrier)"
        ),
    };

    // Each sink must appear in the report (one wake per display).
    // The report's row ORDER is non-deterministic — assert set
    // membership, not Vec equality.
    let reported: std::collections::HashSet<DisplayId> =
        report.displays.iter().map(|r| r.display.clone()).collect();
    let expected: std::collections::HashSet<DisplayId> = displays.iter().cloned().collect();
    assert_eq!(
        reported, expected,
        "every display must appear in the report once"
    );
    for row in &report.displays {
        assert!(row.ok, "row {row:?} should be ok after barrier release");
        assert!(row.error.is_none(), "ok row should have no error: {row:?}");
    }

    // Sanity-check: each sink's `wake_once` actually ran (only happens
    // if its task was spawned and reached the barrier).  The Vec
    // order is non-deterministic, so assert set membership.
    let started_ids = started.lock().expect("started lock poisoned").clone();
    let started_set: std::collections::HashSet<DisplayId> = started_ids.into_iter().collect();
    assert_eq!(
        started_set, expected,
        "every BarrierSink.wake_once must have run — proves the outer \
             handler reached every display"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── exercise handler ───────────────────────────────────────────────────────────
//
// RED-first pin for `ControlMsg::Exercise`: the handler must
// (a) pause the target display's rule(s) BEFORE running the sequence and
//     un-pause them after, so a presence edge cannot race the test;
// (b) skip cleanly for a manual-only display (no rule bound) — the
//     `paused_rules` list is empty and the sequence still runs;
// (c) build a per-step report with one row per phase (read / blank /
//     wake / restore) and surface `Confirmed` / `Unconfirmable` verdicts
//     from the read_state comparison;
// (d) ALWAYS wake the display (the cardinal fail-safe), even when the
//     blank command itself fails — the report's `Failed` verdict on the
//     blank step is the operator's signal, not a stranded-dark panel.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn exercise_handler_pauses_target_rule_and_unpauses_after() {
    use dormant_core::rules::{ExerciseReport, ExerciseVerdict};
    use dormant_core::traits::{PanelState, PowerState};

    // A scripted sink that returns a panel state which moves on every
    // read → every step is `Confirmed` → exercise exits cleanly with the
    // operator's report.
    struct ScriptedSink {
        #[allow(dead_code)]
        id: DisplayId,
        states: Arc<std::sync::Mutex<Vec<PanelState>>>,
        log: Arc<std::sync::Mutex<Vec<SinkCmd>>>,
    }
    #[async_trait::async_trait]
    impl CommandSink for ScriptedSink {
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            self.log
                .lock()
                .expect("log lock poisoned")
                .push(SinkCmd::Blank(BlankMode::PowerOff));
            Ok(())
        }
        async fn wake(&self) -> Result<(), CmdFailure> {
            self.log
                .lock()
                .expect("log lock poisoned")
                .push(SinkCmd::Wake);
            Ok(())
        }
        async fn wake_once(&self) -> Result<(), CmdFailure> {
            self.wake().await
        }
        fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
            Vec::new()
        }
        async fn read_state(&self) -> Option<PanelState> {
            self.states.lock().expect("states lock poisoned").pop()
        }
    }

    let display = DisplayId("mon".into());
    let zones = zone_with_sensor("desk", "office");

    let log = Arc::new(std::sync::Mutex::new(Vec::<SinkCmd>::new()));
    // Script: reads return On, then Standby, then On, then On.  The
    // final "On" is the defensive restore-step read; it's already at
    // baseline, so the wake step's verdict is `Confirmed` (it moved
    // the panel back from Standby → On).
    let states_script: Vec<PanelState> = vec![
        PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        }, // restore-read
        PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        }, // post-wake
        PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(0),
        }, // post-blank
        PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        }, // baseline
    ];
    let states = Arc::new(std::sync::Mutex::new(states_script));
    let sink_arc: Arc<dyn CommandSink> = Arc::new(ScriptedSink {
        id: display.clone(),
        states: Arc::clone(&states),
        log: Arc::clone(&log),
    });

    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink_arc);

    let cfg = RulesEngineConfig {
        rules: vec![rule_cfg("r_office", "office", &["mon"])],
        displays: vec![display_cfg("mon")],
        sensors: vec![sensor_cfg(
            "desk",
            SensorKind::Presence,
            None,
            Duration::from_secs(3600),
        )],
    };

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    let (tx, rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::Exercise {
            display: display.clone(),
            reply: tx,
        })
        .await
        .expect("ctl channel open");
    let report: ExerciseReport = match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_dropped)) => panic!("exercise reply oneshot dropped"),
        Err(elapsed) => panic!("exercise reply did not arrive within 5s: {elapsed:?}"),
    };

    // Rule is reported as paused.
    assert_eq!(
        report.paused_rules,
        vec![RuleId("r_office".into())],
        "exercise must report the rule it paused for the window"
    );

    // Every step in the report is present.  The initial "read" is the
    // baseline capture (always `Unconfirmable` — the read itself doesn't
    // verify a transition, it just records a snapshot).  The blank /
    // wake / restore steps are `Confirmed` because the scripted reads
    // make every comparison succeed.
    let commands: Vec<&str> = report.steps.iter().map(|s| s.command.as_str()).collect();
    assert_eq!(
        commands,
        vec!["read", "blank", "wake", "restore"],
        "expected exactly one row per exercise phase"
    );
    assert_eq!(
        report.steps[0].verdict,
        ExerciseVerdict::Unconfirmable,
        "the baseline read step is informational, not a verification"
    );
    for step in report.steps.iter().skip(1) {
        assert_eq!(
            step.verdict,
            ExerciseVerdict::Confirmed,
            "scripted reads should yield Confirmed for every step: {step:?}"
        );
    }

    // The sink log shows the full sequence: blank, wake, wake (defensive).
    let log_snapshot = log.lock().expect("log lock poisoned").clone();
    assert_eq!(
        log_snapshot,
        vec![
            SinkCmd::Blank(BlankMode::PowerOff),
            SinkCmd::Wake,
            SinkCmd::Wake,
        ],
        "exercise must run blank → wake → defensive wake (in that order)"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

/// Manual-only display path: a display with no rule bound must still be
/// exercisable (the `paused_rules` list is empty), and the per-step
/// report must still ship every phase.  This is the "skip cleanly" branch
/// the design doc calls out.
#[tokio::test(flavor = "current_thread")]
async fn exercise_handler_runs_for_manual_only_display_with_empty_paused_rules() {
    use dormant_core::rules::{ExerciseReport, ExerciseVerdict};
    use dormant_core::traits::PanelState;

    struct EmptySink;
    #[async_trait::async_trait]
    impl CommandSink for EmptySink {
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            Ok(())
        }
        async fn wake(&self) -> Result<(), CmdFailure> {
            Ok(())
        }
        async fn wake_once(&self) -> Result<(), CmdFailure> {
            Ok(())
        }
        fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
            Vec::new()
        }
        async fn read_state(&self) -> Option<PanelState> {
            None // unconfirmable
        }
    }

    let display = DisplayId("manual_only".into());
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), Arc::new(EmptySink));

    // No rules at all — manual-only display.
    let cfg = RulesEngineConfig {
        rules: vec![],
        displays: vec![display_cfg("manual_only")],
        sensors: vec![],
    };
    let zones = ZoneEngine::new(Vec::new(), &[]).expect("empty zone engine");
    let harness = spawn_engine(cfg, zones, execs, vec![]);

    let (tx, rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::Exercise {
            display: display.clone(),
            reply: tx,
        })
        .await
        .expect("ctl channel open");
    let report: ExerciseReport = match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_dropped)) => panic!("exercise reply oneshot dropped"),
        Err(elapsed) => panic!("exercise reply did not arrive within 5s: {elapsed:?}"),
    };

    assert!(
        report.paused_rules.is_empty(),
        "manual-only display must not pause any rule: {:?}",
        report.paused_rules
    );
    // Every step ran and is `Unconfirmable` (no readback).
    assert_eq!(report.steps.len(), 4);
    for step in &report.steps {
        assert_eq!(
            step.verdict,
            ExerciseVerdict::Unconfirmable,
            "no readback → every step is Unconfirmable: {step:?}"
        );
    }

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

// ── wake-failure-surfacing: BlankFailure / BlankRecovered / WakeRecovered ──────
//
// These tests drive the display directly via `ForceBlank` / `ForceWake` (no
// zone/sensor wiring needed) so the scripted `CmdFailure`s land deterministically
// on the exact command we're scripting, mirroring `wake_failure_retries_until_success`'s
// use of `RecordingSink::push_{blank,wake}_result` but without the grace-timing
// dependency that test needs for its own (unrelated) assertions.

/// A manual-only (rule-less) config for a single display — `ForceBlank` /
/// `ForceWake` bypass grace/zone gating entirely, so no rule or sensor
/// wiring is needed to drive the blank/wake command paths directly.
fn manual_cfg(display_id: &str) -> (RulesEngineConfig, ZoneEngine) {
    let cfg = RulesEngineConfig {
        rules: vec![],
        displays: vec![display_cfg(display_id)],
        sensors: vec![],
    };
    let zones = ZoneEngine::new(Vec::new(), &[]).expect("empty zone engine is valid");
    (cfg, zones)
}

#[tokio::test(start_paused = true)]
async fn chain_exhausted_blank_emits_blank_failure_and_sets_snapshot_flag() {
    let display = DisplayId("mon".into());

    let (cfg, zones) = manual_cfg("mon");
    let sink = Arc::new(RecordingSink::new());
    sink.push_blank_result(Err(CmdFailure {
        controller: "cmd".into(),
        error: "E_BLANK_FAILED: boom".into(),
    }));
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    harness
        .ctl_tx
        .send(ControlMsg::ForceBlank(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut saw_blank_failure = None;
    while let Ok(ev) = events_rx.try_recv() {
        if let DaemonEvent::BlankFailure {
            display: d,
            controller,
            detail,
        } = ev
        {
            saw_blank_failure = Some((d, controller, detail));
        }
    }
    let (d, controller, detail) =
        saw_blank_failure.expect("expected a BlankFailure event on the stream");
    assert_eq!(d, display);
    assert_eq!(controller, "cmd");
    assert_eq!(detail, "E_BLANK_FAILED: boom");

    let snap = request_snapshot(&harness.ctl_tx).await;
    let d_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("display in snapshot");
    assert!(
        d_snap.1.last_blank_failed,
        "snapshot must show last_blank_failed=true after chain-exhausted blank"
    );
    assert_eq!(d_snap.1.wake_attempts, 0);

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

#[tokio::test(start_paused = true)]
async fn blank_recovery_emits_once_and_clears_flag() {
    let display = DisplayId("mon".into());

    let (cfg, zones) = manual_cfg("mon");
    let sink = Arc::new(RecordingSink::new());
    sink.push_blank_result(Err(CmdFailure {
        controller: "cmd".into(),
        error: "E_BLANK_FAILED: boom".into(),
    }));
    sink.push_blank_result(Ok(()));
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    // First ForceBlank: scripted failure — enters BlankFailure state, and
    // (per the blank-failure state-machine path with no zone bound) the
    // display returns to Active, so a second ForceBlank can drive the next
    // (successful) blank attempt.
    harness
        .ctl_tx
        .send(ControlMsg::ForceBlank(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(150)).await;

    harness
        .ctl_tx
        .send(ControlMsg::ForceBlank(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut blank_recovered_count = 0;
    while let Ok(ev) = events_rx.try_recv() {
        if matches!(ev, DaemonEvent::BlankRecovered { .. }) {
            blank_recovered_count += 1;
        }
    }
    assert_eq!(
        blank_recovered_count, 1,
        "expected exactly one BlankRecovered event"
    );

    let snap = request_snapshot(&harness.ctl_tx).await;
    let d_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("display in snapshot");
    assert!(
        !d_snap.1.last_blank_failed,
        "snapshot flag must clear after recovery"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

/// Anti-tautology pin (spec §9): a display's very first ever blank/wake
/// attempt succeeding must NOT be reported as a "recovery" — recovery only
/// makes sense relative to a prior recorded failure.
#[tokio::test(start_paused = true)]
async fn first_ever_success_emits_no_recovery() {
    let display = DisplayId("mon".into());

    let (cfg, zones) = manual_cfg("mon");
    let sink = Arc::new(RecordingSink::new());
    // No scripted results — every call defaults to Ok.
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    harness
        .ctl_tx
        .send(ControlMsg::ForceBlank(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(100)).await;

    harness
        .ctl_tx
        .send(ControlMsg::ForceWake(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(100)).await;

    while let Ok(ev) = events_rx.try_recv() {
        assert!(
            !matches!(
                ev,
                DaemonEvent::BlankRecovered { .. } | DaemonEvent::WakeRecovered { .. }
            ),
            "first-ever success must not be reported as a recovery, got {ev:?}"
        );
    }

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

#[tokio::test(start_paused = true)]
async fn wake_recovery_carries_attempt_count() {
    let display = DisplayId("mon".into());

    let (cfg, zones) = manual_cfg("mon");
    let sink = Arc::new(RecordingSink::new());
    for _ in 0..3 {
        sink.push_wake_result(Err(CmdFailure {
            controller: "mon".into(),
            error: "E_DISPLAY_IO: timeout".into(),
        }));
    }
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let harness = spawn_engine(cfg, zones, execs, vec![]);

    let (sub_tx, sub_rx) = oneshot::channel();
    harness
        .ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx = sub_rx.await.expect("subscribe reply");

    // Blank first (default Ok) so the display is Blanked, then ForceWake —
    // Waking's WakeResult(Err) handler immediately re-issues wake with a
    // fresh generation (state_machine.rs: "the executor's own burst already
    // backed off"), so all 3 scripted failures plus the final success drain
    // within a handful of executor task turns — no virtual-clock advance
    // needed.
    harness
        .ctl_tx
        .send(ControlMsg::ForceBlank(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(100)).await;

    harness
        .ctl_tx
        .send(ControlMsg::ForceWake(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut retry_count = 0;
    let mut wake_recovered_attempts = None;
    while let Ok(ev) = events_rx.try_recv() {
        match ev {
            DaemonEvent::WakeRetry { .. } => retry_count += 1,
            DaemonEvent::WakeRecovered { attempts, .. } => wake_recovered_attempts = Some(attempts),
            _ => {}
        }
    }
    assert_eq!(retry_count, 3, "expected 3 WakeRetry broadcasts");
    assert_eq!(
        wake_recovered_attempts,
        Some(3),
        "expected WakeRecovered{{ attempts: 3 }}"
    );

    let snap = request_snapshot(&harness.ctl_tx).await;
    let d_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("display in snapshot");
    assert_eq!(
        d_snap.1.wake_attempts, 0,
        "wake_attempts must reset to 0 after recovery"
    );

    harness.cancel.cancel();
    let _ = harness.engine_handle.await;
    let _ = harness.source_handle.await;
}

/// `RulesEngine::seed_failure_state` is the seam a future reload/restore
/// path (T3) uses to carry failure bookkeeping across an engine rebuild.
/// Since `spawn_engine` constructs the `RulesEngine` internally, this test
/// builds the engine + task inline (mirroring `spawn_engine`'s body) so it
/// can seed pre-run, exactly as a real caller must (there is no post-spawn
/// seeding control message — seeding is a pre-run-only mutator, like
/// `apply_restore_effects`).
#[tokio::test(start_paused = true)]
async fn seeded_failure_state_surfaces_in_next_snapshot() {
    let display = DisplayId("mon".into());

    let (cfg, zones) = manual_cfg("mon");
    let sink = Arc::new(RecordingSink::new());
    sink.push_wake_result(Ok(()));
    let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
    execs.insert(display.clone(), sink.clone());

    let (events_tx, events_rx) = mpsc::channel(64);
    let (ctl_tx, ctl_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();

    let mut engine = RulesEngine::new(
        cfg,
        zones,
        execs,
        HashMap::new(),
        Arc::new(dormant_core::ownership::AlwaysOwned),
    )
    .expect("valid engine config");

    // Seed pre-run: as if a prior engine generation recorded 5 wake
    // attempts and a still-open blank failure for this display.
    engine.seed_failure_state(&display, 5, true);

    // Kept alive for the whole test (like `Harness::events_tx`) — the
    // engine's `run()` loop breaks its main select on `events.recv() ==
    // None`, so dropping this early would end the run before our ctl
    // messages are processed.  This test drives everything via `ctl_tx` /
    // `ForceBlank` / `ForceWake`; no sensor script is needed.
    let _events_tx = events_tx;

    let engine_cancel = cancel.clone();
    let engine_handle = tokio::spawn(async move {
        engine.run(events_rx, ctl_rx, engine_cancel).await;
    });

    let (sub_tx, sub_rx) = oneshot::channel();
    ctl_tx
        .send(ControlMsg::SubscribeEvents(sub_tx))
        .await
        .expect("ctl open");
    let mut events_rx_sub = sub_rx.await.expect("subscribe reply");

    let snap = request_snapshot(&ctl_tx).await;
    let d_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("display in snapshot");
    assert_eq!(
        d_snap.1.wake_attempts, 5,
        "seeded wake_attempts must surface"
    );
    assert!(
        d_snap.1.last_blank_failed,
        "seeded last_blank_failed must surface"
    );

    // A subsequent successful wake must report the seeded attempt count.
    ctl_tx
        .send(ControlMsg::ForceBlank(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(100)).await;
    ctl_tx
        .send(ControlMsg::ForceWake(display.clone()))
        .await
        .expect("ctl open");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut wake_recovered_attempts = None;
    while let Ok(ev) = events_rx_sub.try_recv() {
        if let DaemonEvent::WakeRecovered { attempts, .. } = ev {
            wake_recovered_attempts = Some(attempts);
        }
    }
    assert_eq!(
        wake_recovered_attempts,
        Some(5),
        "WakeRecovered must carry the seeded attempt count"
    );

    cancel.cancel();
    let _ = engine_handle.await;
}
