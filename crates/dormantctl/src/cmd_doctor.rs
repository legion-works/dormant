//! `dormantctl doctor` — hardware/connectivity verification.
//!
//! Runs probes against configured sensors, displays, and credentials to
//! diagnose connectivity and capability issues without needing a running
//! daemon.  Each probe returns a [`ProbeResult`]; the bare `doctor` command
//! runs every probe applicable to the loaded config and prints a ✓/✗ table.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use comfy_table::Color;
use comfy_table::ContentArrangement;
use comfy_table::{Cell, Row, Table};
use dormant_core::config::schema::{Credentials, HaSensorCfg, MqttSensorCfg, SensorConfig};
use dormant_core::config::{Strictness, load_config, load_credentials, validate};
use dormant_core::error::E_HA_AUTH;
use dormant_core::paths;
use dormant_core::types::{SensorId, SensorState};
use dormant_displays::registry::capabilities;
#[cfg(target_os = "linux")]
use dormant_displays::vcp_ops::RealVcp;
use dormant_displays::vcp_ops::VcpOps;
use dormant_sensors::ha_ws::{Action, HaProtocol};
use dormant_sensors::mqtt::parse_payload;
use dormant_sensors::usb_ld2410::FrameParser;
use futures_util::{SinkExt, StreamExt};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use tokio_tungstenite::tungstenite::Message;

// ── DoctorOutcome ───────────────────────────────────────────────────────────────

/// The outcome of a `doctor` invocation: exit code + optional message.
#[derive(Debug, Clone, PartialEq)]
pub enum DoctorOutcome {
    /// All probes passed or were skipped.
    AllOk,
    /// At least one probe failed.
    SomeFailed,
    /// The subcommand is not yet supported (exit 3).
    NotSupported(String),
}

// ── ProbeResult ─────────────────────────────────────────────────────────────────

/// The outcome of a single probe.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    /// Human-readable probe name (e.g. `"ddcci"`, `"usb /dev/ttyUSB0"`).
    pub name: String,
    /// Probe status.
    pub status: ProbeStatus,
    /// Optional detail message.
    pub detail: String,
}

/// Probe status.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeStatus {
    /// Probe succeeded.
    Pass,
    /// Probe failed.
    Fail,
    /// Probe was skipped (no applicable config).
    Skip,
}

impl ProbeResult {
    /// Create a passing probe result.
    #[must_use]
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProbeStatus::Pass,
            detail: detail.into(),
        }
    }

    /// Create a failing probe result.
    #[must_use]
    pub fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProbeStatus::Fail,
            detail: detail.into(),
        }
    }

    /// Create a skipped probe result.
    #[must_use]
    pub fn skip(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProbeStatus::Skip,
            detail: detail.into(),
        }
    }
}

// ── CLI ─────────────────────────────────────────────────────────────────────────

/// Diagnose hardware and connectivity.
#[derive(Parser, Debug)]
pub struct DoctorArgs {
    /// Path to the config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Path to the credentials file.
    #[arg(long)]
    pub credentials: Option<PathBuf>,

    #[command(subcommand)]
    pub subcommand: Option<DoctorSubcommand>,
}

#[derive(clap::Subcommand, Debug)]
pub enum DoctorSubcommand {
    /// Probe DDC/CI displays.
    Ddcci,
    /// Probe a USB LD2410 radar sensor.
    Usb {
        /// Serial port path (e.g. `/dev/ttyUSB0`).
        port: String,
        /// Baud rate (default 256000).
        #[arg(long, default_value = "256000")]
        baud: u32,
    },
    /// Probe MQTT sensors.
    Mqtt,
    /// Probe Home Assistant WebSocket sensors.
    Ha,
    /// Validate configuration.
    Config,
    /// Probe `KWin` DPMS (not yet supported).
    Kwin,
    /// Probe Samsung Tizen display (not yet supported).
    Samsung,
}

// ── Run ─────────────────────────────────────────────────────────────────────────

/// Run the `doctor` command.
///
/// # Errors
///
/// Propagates I/O and config-loading errors.
pub fn run(args: &DoctorArgs) -> Result<DoctorOutcome> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(args))
}

async fn run_async(args: &DoctorArgs) -> Result<DoctorOutcome> {
    match &args.subcommand {
        Some(DoctorSubcommand::Ddcci) => {
            #[cfg(target_os = "linux")]
            {
                let results = vec![probe_ddcci().await];
                print_table(&results);
                Ok(outcome(&results))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Ok(DoctorOutcome::NotSupported("ddcci".into()))
            }
        }
        Some(DoctorSubcommand::Usb { port, baud }) => {
            let results = vec![probe_usb(port, *baud).await];
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Mqtt) => {
            let (cfg, _creds) = load_config_and_creds(args)?;
            let results = probe_mqtt_all(&cfg).await;
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Ha) => {
            let (cfg, creds) = load_config_and_creds(args)?;
            let results = probe_ha_all(&cfg, &creds).await;
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Config) => {
            let results = probe_config(args);
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Kwin) => Ok(DoctorOutcome::NotSupported("kwin-dpms".into())),
        Some(DoctorSubcommand::Samsung) => Ok(DoctorOutcome::NotSupported("samsung-tizen".into())),
        None => {
            // Bare doctor: run everything applicable.
            let (cfg, creds) = load_config_and_creds(args)?;
            let mut results = Vec::new();

            // Config probe first.
            let config_result = probe_config_inner(&cfg, &creds);
            let config_ok = config_result.status != ProbeStatus::Fail;
            results.push(config_result);

            // Collect sensor probes.
            let mut sensor_futs: Vec<
                std::pin::Pin<Box<dyn futures_util::Future<Output = ProbeResult>>>,
            > = Vec::new();
            for (id, sensor_cfg) in &cfg.sensors {
                if !config_ok {
                    // Skip dependent probes when config is invalid.
                    let name = match sensor_cfg {
                        SensorConfig::Mqtt(_) => format!("mqtt {id}"),
                        SensorConfig::Ha(_) => format!("ha {id}"),
                        SensorConfig::UsbLd2410(usb_cfg) => format!("usb {}", usb_cfg.port),
                    };
                    results.push(ProbeResult::skip(name, "config invalid — fix config first"));
                    continue;
                }
                match sensor_cfg {
                    SensorConfig::Mqtt(mqtt_cfg) => {
                        let id = id.clone();
                        let cfg = mqtt_cfg.clone();
                        sensor_futs.push(Box::pin(async move { probe_mqtt_one(&id, &cfg).await }));
                    }
                    SensorConfig::Ha(ha_cfg) => {
                        let id = id.clone();
                        let cfg = ha_cfg.clone();
                        let creds = creds.clone();
                        sensor_futs.push(Box::pin(
                            async move { probe_ha_one(&id, &cfg, &creds).await },
                        ));
                    }
                    SensorConfig::UsbLd2410(usb_cfg) => {
                        let port = usb_cfg.port.clone();
                        let baud = usb_cfg.baud;
                        sensor_futs.push(Box::pin(async move { probe_usb(&port, baud).await }));
                    }
                }
            }

            // Run sensor probes in parallel.
            if !sensor_futs.is_empty() {
                let sensor_results = futures_util::future::join_all(sensor_futs).await;
                results.extend(sensor_results);
            }

            // DDC/CI probe if any display uses ddcci (serial after sensors).
            #[cfg(target_os = "linux")]
            if config_ok {
                let has_ddcci = cfg
                    .displays
                    .values()
                    .any(|d| d.controllers.iter().any(|c| c == "ddcci"));
                if has_ddcci {
                    results.push(probe_ddcci().await);
                }
            }

            print_table(&results);
            Ok(outcome(&results))
        }
    }
}

// ── Config loading ──────────────────────────────────────────────────────────────

/// Load config and credentials using the same default-path logic as `validate`.
fn load_config_and_creds(args: &DoctorArgs) -> Result<(dormant_core::config::Config, Credentials)> {
    let config_path =
        paths::resolve_config_path(args.config.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let creds_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| paths::sibling_credentials(&config_path));

    let (cfg, _warnings) = load_config(&config_path, Strictness::Warn)?;
    let creds = load_credentials(&creds_path)?;
    Ok((cfg, creds))
}

// ── Probe: config ───────────────────────────────────────────────────────────────

fn probe_config(args: &DoctorArgs) -> Vec<ProbeResult> {
    let (cfg, creds) = match load_config_and_creds(args) {
        Ok(pair) => pair,
        Err(e) => return vec![ProbeResult::fail("config", format!("{e:#}"))],
    };
    vec![probe_config_inner(&cfg, &creds)]
}

fn probe_config_inner(cfg: &dormant_core::config::Config, creds: &Credentials) -> ProbeResult {
    let errors = validate(cfg, &capabilities(), creds);
    if errors.is_empty() {
        ProbeResult::pass("config", "configuration OK")
    } else {
        let detail: String = errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        ProbeResult::fail("config", detail)
    }
}

// ── Probe: DDC/CI ───────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
async fn probe_ddcci() -> ProbeResult {
    let ops = RealVcp;
    let displays = ops.list_displays().await;

    if displays.is_empty() {
        return ProbeResult::fail("ddcci", "no DDC/CI displays detected");
    }

    let mut details: Vec<String> = Vec::new();
    let mut all_ok = true;

    for display in &displays {
        let ident = &display.ident_string;
        let brightness = ops.get_vcp(ident, 0x10).await;
        let d6 = ops.get_vcp(ident, 0xD6).await;

        let mut line = format!("  {ident}: brightness=");
        match brightness {
            Ok(v) => {
                line.push_str(&v.to_string());
            }
            Err(e) => {
                use std::fmt::Write;
                let _ = write!(line, "ERR({e})");
                all_ok = false;
            }
        }
        line.push_str(", power_control=");
        match d6 {
            Ok(_) => line.push_str("supported"),
            Err(_) => line.push_str("not supported"),
        }
        details.push(line);
    }

    let detail = details.join("\n");
    if all_ok {
        ProbeResult::pass("ddcci", detail)
    } else {
        ProbeResult::fail("ddcci", detail)
    }
}

// ── Probe: USB LD2410 ───────────────────────────────────────────────────────────

async fn probe_usb(port: &str, baud: u32) -> ProbeResult {
    use tokio::io::AsyncReadExt;

    let builder = tokio_serial::new(port, baud)
        .data_bits(tokio_serial::DataBits::Eight)
        .stop_bits(tokio_serial::StopBits::One)
        .parity(tokio_serial::Parity::None);

    let mut stream = match tokio_serial::SerialStream::open(&builder) {
        Ok(s) => s,
        Err(e) => {
            return ProbeResult::fail(format!("usb {port}"), format!("failed to open port: {e}"));
        }
    };

    let mut parser = FrameParser::new();
    let mut buf = [0u8; 256];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut total_frames = 0usize;
    let mut last_state: Option<SensorState> = None;

    while tokio::time::Instant::now() < deadline {
        let timeout = deadline - tokio::time::Instant::now();
        let result = tokio::time::timeout(timeout, stream.read(&mut buf)).await;

        match result {
            Ok(Ok(0)) => {
                return ProbeResult::fail(
                    format!("usb {port}"),
                    "port returned EOF (device disconnected)".to_string(),
                );
            }
            Ok(Ok(n)) => {
                let frames = parser.push(&buf[..n]);
                total_frames += frames.len();
                for frame in frames {
                    let state = if frame.target_state == 0 {
                        SensorState::Absent
                    } else {
                        SensorState::Present
                    };
                    last_state = Some(state);
                }
            }
            Ok(Err(e)) => {
                return ProbeResult::fail(format!("usb {port}"), format!("read error: {e}"));
            }
            Err(_elapsed) => break, // timeout
        }
    }

    if total_frames == 0 {
        return ProbeResult::fail(
            format!("usb {port}"),
            "port opened but no LD2410 frames decoded (wrong port? wrong baud?)".to_string(),
        );
    }

    let state_str = match last_state {
        Some(SensorState::Present) => "present",
        Some(SensorState::Absent) => "absent",
        Some(SensorState::Unavailable) => "unavailable",
        None => "unknown",
    };

    ProbeResult::pass(
        format!("usb {port}"),
        format!("{total_frames} frames decoded, last state: {state_str}"),
    )
}

// ── Probe: MQTT ─────────────────────────────────────────────────────────────────

async fn probe_mqtt_all(cfg: &dormant_core::config::Config) -> Vec<ProbeResult> {
    let mut results = Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        if let SensorConfig::Mqtt(mqtt_cfg) = sensor_cfg {
            results.push(probe_mqtt_one(id, mqtt_cfg).await);
        }
    }
    if results.is_empty() {
        results.push(ProbeResult::skip("mqtt", "no MQTT sensors configured"));
    }
    results
}

async fn probe_mqtt_one(id: &str, cfg: &MqttSensorCfg) -> ProbeResult {
    let name = format!("mqtt {id}");

    // Parse broker URL.
    let broker_url = &cfg.broker_url;
    let (host, port) = parse_broker_url(broker_url);

    // Build a temporary MQTT client.
    let client_id = format!("dormant-doctor-{id}-{}", std::process::id());
    let mut mqttopts = MqttOptions::new(&client_id, host, port);
    mqttopts.set_clean_session(true);
    let (client, mut eventloop) = AsyncClient::new(mqttopts, 100);

    // Subscribe to the sensor topic.
    if let Err(e) = client.subscribe(&cfg.topic, QoS::AtLeastOnce).await {
        return ProbeResult::fail(name, format!("subscribe failed: {e}"));
    }

    // Wait up to 10s for a retained/live message.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut found: Option<SensorState> = None;
    let mut broker_connected = false;

    while tokio::time::Instant::now() < deadline {
        let timeout = deadline - tokio::time::Instant::now();
        let result = tokio::time::timeout(timeout, eventloop.poll()).await;

        match result {
            Ok(Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)))) => {
                let state = parse_payload(cfg, &publish.payload);
                if let Some(s) = state {
                    found = Some(s);
                    break;
                }
            }
            Ok(Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_)))) => {
                broker_connected = true;
            }
            Ok(Ok(_)) => {} // Ignore other packets.
            Ok(Err(e)) => {
                return ProbeResult::fail(name, format!("broker connection error: {e}"));
            }
            Err(_elapsed) => break, // timeout
        }
    }

    let _ = client.disconnect().await;

    match found {
        Some(state) => {
            let state_str = match state {
                SensorState::Present => "present",
                SensorState::Absent => "absent",
                SensorState::Unavailable => "unavailable",
            };
            ProbeResult::pass(name, format!("topic '{}' reports {state_str}", cfg.topic))
        }
        None => {
            if broker_connected {
                ProbeResult::skip(
                    name,
                    format!(
                        "no message in 10s on '{}' — on-change sensors are quiet when state is stable; not a failure",
                        cfg.topic,
                    ),
                )
            } else {
                ProbeResult::fail(
                    name,
                    "broker connection failed (no CONNACK received)".to_string(),
                )
            }
        }
    }
}

/// Parse a broker URL into (host, port).
fn parse_broker_url(url: &str) -> (&str, u16) {
    let rest = url
        .strip_prefix("tcp://")
        .or_else(|| url.strip_prefix("mqtt://"))
        .unwrap_or(url);
    if let Some((host, port_str)) = rest.rsplit_once(':')
        && let Ok(port) = port_str.parse::<u16>()
    {
        return (host, port);
    }
    (rest, 1883)
}

// ── Probe: HA WebSocket ─────────────────────────────────────────────────────────

async fn probe_ha_all(cfg: &dormant_core::config::Config, creds: &Credentials) -> Vec<ProbeResult> {
    let mut results = Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        if let SensorConfig::Ha(ha_cfg) = sensor_cfg {
            results.push(probe_ha_one(id, ha_cfg, creds).await);
        }
    }
    if results.is_empty() {
        results.push(ProbeResult::skip("ha", "no HA sensors configured"));
    }
    results
}

async fn probe_ha_one(id: &str, cfg: &HaSensorCfg, creds: &Credentials) -> ProbeResult {
    let name = format!("ha {id}");

    let token = match &creds.ha_token {
        Some(t) => t.clone(),
        None => {
            return ProbeResult::fail(name, "no ha_token configured in credentials".to_string());
        }
    };

    // Connect WebSocket.
    let ws_result = tokio_tungstenite::connect_async(&cfg.url).await;
    let (ws_stream, _) = match ws_result {
        Ok(pair) => pair,
        Err(e) => {
            return ProbeResult::fail(name, format!("WebSocket connection failed: {e}"));
        }
    };

    let (mut write, read) = ws_stream.split();
    let mut protocol = HaProtocol::new(token, vec![(SensorId(id.into()), cfg.entity.clone())]);

    let mut read = read;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut found_state: Option<SensorState> = None;
    let mut auth_failure: Option<String> = None;

    while tokio::time::Instant::now() < deadline {
        let timeout = deadline - tokio::time::Instant::now();
        let msg = tokio::time::timeout(timeout, read.next()).await;

        match msg {
            Ok(Some(Ok(Message::Text(text)))) => {
                let actions = protocol.handle_message(&text);
                for action in actions {
                    match action {
                        Action::SendText(out) => {
                            if write.send(Message::Text(out)).await.is_err() {
                                return ProbeResult::fail(
                                    name,
                                    "WebSocket write failed".to_string(),
                                );
                            }
                        }
                        Action::Emit(event) => {
                            found_state = Some(event.state);
                        }
                        Action::Fatal(reason) => {
                            auth_failure = Some(reason);
                        }
                        Action::Nothing => {}
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_))) | None) => break,
            Ok(Some(Ok(_))) => {} // ping/pong/binary — ignore
            Ok(Some(Err(e))) => {
                return ProbeResult::fail(name, format!("WebSocket error: {e}"));
            }
            Err(_elapsed) => break, // timeout
        }

        if found_state.is_some() || auth_failure.is_some() {
            break;
        }
    }

    if let Some(state) = found_state {
        let state_str = match state {
            SensorState::Present => "present",
            SensorState::Absent => "absent",
            SensorState::Unavailable => "unavailable",
        };
        ProbeResult::pass(name, format!("entity '{}' reports {state_str}", cfg.entity))
    } else if let Some(reason) = auth_failure {
        if reason.contains(E_HA_AUTH) {
            ProbeResult::fail(name, format!("{reason} (check ha_token)"))
        } else {
            ProbeResult::fail(name, reason)
        }
    } else {
        ProbeResult::fail(
            name,
            format!("no state received from entity '{}' within 10s", cfg.entity),
        )
    }
}

// ── Table printing ──────────────────────────────────────────────────────────────

/// Print a table of probe results.
fn print_table(results: &[ProbeResult]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Probe", "Status", "Detail"]);

    for r in results {
        let (glyph, color) = match r.status {
            ProbeStatus::Pass => ("✓", Color::Green),
            ProbeStatus::Fail => ("✗", Color::Red),
            ProbeStatus::Skip => ("-", Color::Yellow),
        };
        table.add_row(Row::from(vec![
            Cell::new(&r.name),
            Cell::new(glyph).fg(color),
            Cell::new(&r.detail),
        ]));
    }

    println!("{table}");
}

/// Determine the overall outcome from probe results.
fn outcome(results: &[ProbeResult]) -> DoctorOutcome {
    if results.iter().any(|r| r.status == ProbeStatus::Fail) {
        DoctorOutcome::SomeFailed
    } else {
        DoctorOutcome::AllOk
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ProbeResult table formatting ────────────────────────────────────────

    #[test]
    fn table_contains_glyphs() {
        let results = vec![
            ProbeResult::pass("test-pass", "all good"),
            ProbeResult::fail("test-fail", "something broke"),
            ProbeResult::skip("test-skip", "not applicable"),
        ];

        // Print to string and check glyphs.
        let mut table = Table::new();
        table
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["Probe", "Status", "Detail"]);
        for r in &results {
            let (glyph, color) = match r.status {
                ProbeStatus::Pass => ("✓", Color::Green),
                ProbeStatus::Fail => ("✗", Color::Red),
                ProbeStatus::Skip => ("-", Color::Yellow),
            };
            table.add_row(Row::from(vec![
                Cell::new(&r.name),
                Cell::new(glyph).fg(color),
                Cell::new(&r.detail),
            ]));
        }

        let output = table.to_string();
        assert!(output.contains('✓'), "table should contain checkmark");
        assert!(output.contains('✗'), "table should contain X mark");
        assert!(
            output.contains("test-pass"),
            "table should contain probe name"
        );
        assert!(
            output.contains("test-fail"),
            "table should contain probe name"
        );
        assert!(
            output.contains("test-skip"),
            "table should contain probe name"
        );
    }

    // ── ProbeResult construction ────────────────────────────────────────────

    #[test]
    fn probe_result_pass() {
        let r = ProbeResult::pass("ddcci", "2 displays found");
        assert_eq!(r.name, "ddcci");
        assert_eq!(r.status, ProbeStatus::Pass);
        assert_eq!(r.detail, "2 displays found");
    }

    #[test]
    fn probe_result_fail() {
        let r = ProbeResult::fail("usb /dev/ttyUSB0", "port not found");
        assert_eq!(r.status, ProbeStatus::Fail);
    }

    #[test]
    fn probe_result_skip() {
        let r = ProbeResult::skip("mqtt", "no MQTT sensors");
        assert_eq!(r.status, ProbeStatus::Skip);
    }

    // ── parse_broker_url ────────────────────────────────────────────────────

    #[test]
    fn parse_broker_url_tcp() {
        let (host, port) = parse_broker_url("tcp://mqtt.local:1883");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_plain() {
        let (host, port) = parse_broker_url("127.0.0.1:1883");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_default_port() {
        let (host, port) = parse_broker_url("mqtt.local");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    // ── HA probe: protocol-driven tests ─────────────────────────────────────

    /// Test that the HA probe's message-handling path works with canned
    /// messages (same pattern as `HaProtocol` tests).
    #[test]
    fn ha_probe_protocol_auth_flow() {
        let mut proto = HaProtocol::new(
            "test_token".into(),
            vec![(
                SensorId("motion".into()),
                "binary_sensor.test_motion".into(),
            )],
        );

        // auth_required → SendText
        let actions = proto.handle_message(r#"{"type":"auth_required"}"#);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::SendText(_)));

        // auth_ok → SendText (subscribe)
        let actions = proto.handle_message(r#"{"type":"auth_ok"}"#);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::SendText(_)));

        // result success → Nothing
        let actions = proto.handle_message(r#"{"id":1,"type":"result","success":true}"#);
        assert_eq!(actions, vec![Action::Nothing]);

        // event with "on" state → Emit Present
        let actions = proto.handle_message(
            r#"{"id":2,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"on"}}}}"#,
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Emit(event) => {
                assert_eq!(event.sensor_id, SensorId("motion".into()));
                assert_eq!(event.state, SensorState::Present);
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn ha_probe_protocol_auth_failure() {
        let mut proto = HaProtocol::new(
            "bad_token".into(),
            vec![(
                SensorId("motion".into()),
                "binary_sensor.test_motion".into(),
            )],
        );

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);

        let actions =
            proto.handle_message(r#"{"type":"auth_invalid","message":"Invalid password"}"#);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Fatal(msg) => {
                assert!(
                    msg.contains(E_HA_AUTH),
                    "fatal message should contain E_HA_AUTH: {msg}"
                );
                assert!(msg.contains("Invalid password"));
            }
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    // ── USB probe: FrameParser tests ────────────────────────────────────────

    #[test]
    fn usb_frame_parser_decodes_present() {
        let mut parser = FrameParser::new();
        let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
        let data_len: u16 = 9;
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.push(0x02); // type = normal
        buf.push(0xAA); // head marker
        buf.push(0x01); // target_state = moving (present)
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf.push(0x55);
        buf.push(0x00);
        buf.extend_from_slice(&[0xF8, 0xF7, 0xF6, 0xF5]);

        let frames = parser.push(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].target_state, 0x01);
    }

    #[test]
    fn usb_frame_parser_decodes_absent() {
        let mut parser = FrameParser::new();
        let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
        let data_len: u16 = 9;
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.push(0x02);
        buf.push(0xAA);
        buf.push(0x00); // target_state = none (absent)
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf.push(0x55);
        buf.push(0x00);
        buf.extend_from_slice(&[0xF8, 0xF7, 0xF6, 0xF5]);

        let frames = parser.push(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].target_state, 0x00);
    }

    // ── DoctorOutcome ───────────────────────────────────────────────────────

    #[test]
    fn outcome_all_pass_returns_all_ok() {
        let results = [ProbeResult::pass("a", ""), ProbeResult::skip("b", "")];
        assert_eq!(outcome(&results), DoctorOutcome::AllOk);
    }

    #[test]
    fn outcome_any_fail_returns_some_failed() {
        let results = [ProbeResult::pass("a", ""), ProbeResult::fail("b", "broken")];
        assert_eq!(outcome(&results), DoctorOutcome::SomeFailed);
    }

    #[test]
    fn outcome_all_skip_returns_all_ok() {
        let results = [ProbeResult::skip("a", "no config")];
        assert_eq!(outcome(&results), DoctorOutcome::AllOk);
    }
}
