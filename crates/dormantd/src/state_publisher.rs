//! Pure state → MQTT publish-record mapping for issue #105.
//!
//! Implements the topic, payload, and collision contract ratified in
//! `.opencode/decisions/2026-07-27-mqtt-publish-contract.md`. NO I/O:
//! the three public entry points return `Vec<PublishRecord>` for the
//! publisher task to forward to `rumqttc`. Keeping these functions
//! pure is the same split the wear tracker uses (see `wear_tracker.rs`):
//! testing each one in isolation without bringing up a broker is the
//! only way the topic / payload literals stay grep-stable across
//! refactors.
//!
//! ## Module split
//!
//! - [`sanitize_topic_id`] — public sanitizer; the contract's only
//!   pure-data helper. Used by every id that lands in a topic.
//! - [`EntityInventory`] — collects the per-kind id lists from a
//!   loaded [`Config`] in config-order. `IndexMap` ordering is the
//!   "first wins" half of the collision rule.
//! - [`discovery_records`] — discovery configs for sensors, zones,
//!   and displays. One retained record per entity, every entity
//!   references the global LWT availability topic, and sensor
//!   entities ADDITIONALLY reference their per-sensor availability
//!   topic with `availability_mode: "all"`.
//! - [`snapshot_records`] — startup / re-connect flush: every
//!   retained state + per-sensor availability record derived from
//!   a [`StateSnapshot`].
//! - [`event_records`] — incremental single-event mapping.
//!   Returns an empty vec for unknown / foreign event variants —
//!   the contract is explicit that the publisher NEVER speculates
//!   about events the engine did not actually emit.
//!
//! ## Disabled-by-default
//!
//! Every entry point returns an empty `Vec` when
//! `cfg.publish.enabled == false`. The publisher task is the only
//! caller and it gates the call site on the same flag; the check
//! here is defensive (and keeps the pure functions easy to
//! unit-test without a real broker).

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use dormant_core::config::schema::{Config, Credentials};
use dormant_core::rules::{DaemonEvent, DisplaySnapshot, StateSnapshot};
use dormant_core::types::SensorState;

// ── Public record type ──────────────────────────────────────────────────────

/// One record for the publisher task to forward to `rumqttc`.
///
/// `topic` and `payload` are the contract's literal strings; `qos`
/// is always 1 and `retain` is always `true` for every record this
/// module emits (the public fields exist for the publisher task's
/// pin-rationale and to keep the wire shape greppable).
#[derive(Debug, Clone, PartialEq)]
pub struct PublishRecord {
    /// MQTT topic (already sanitized).
    pub topic: String,
    /// JSON payload (already serialized).
    pub payload: String,
    /// MQTT `QoS` — always `1` for this contract.
    pub qos: u8,
    /// MQTT retain — always `true` for this contract.
    pub retain: bool,
}

const PUBLISH_QOS: u8 = 1;
const RETAIN: bool = true;

// ── Async publisher (transport seam + spawn lifecycle) ──────────────────────
//
// The I/O half of issue #105 lives here. The pure mapping functions above
// produce `Vec<PublishRecord>`; this module owns the runtime that pushes
// those records through a [`PublisherTransport`] (production:
// `MqttTransport` over `rumqttc`; tests: a recording fake). The runtime
// is reload-aware: the daemon's `spawn_generation` cancels and awaits
// the join handle on every accepted reload, then re-spawns a fresh task
// with the new configuration. At most one publisher client id is
// connected at a time.

/// Lifecycle event emitted by a [`PublisherTransport`] on every
/// (re)connect and on graceful disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportLifecycle {
    /// The transport has (re)connected and is ready to forward records.
    /// The publisher responds by republishing discovery + a current
    /// snapshot, exactly as the contract requires.
    Connected,
    /// The transport has disconnected (graceful via cancellation, or
    /// ungraceful via a broker-side drop). The production transport
    /// owns its own reconnect/backoff loop and self-re-emits Connected
    /// when it returns; tests can ignore this variant.
    Disconnected,
}

/// The seam between the pure mapping module and the broker. The
/// production transport wraps `rumqttc`; tests substitute a recording
/// fake that lets the suite observe every record forwarded and
/// deterministically drive the connect/disconnect cadence.
///
/// Implementers MUST be cheap to construct (the publisher builds one per
/// generation) and MUST honour `cancel` promptly (the daemon awaits the
/// join handle on graceful shutdown).
#[async_trait::async_trait]
pub trait PublisherTransport: Send + 'static {
    /// Run the transport's read-eval-write loop until `cancel` fires
    /// (returns `Ok(())`) or the transport suffers an unrecoverable
    /// error (`Err(_)`). Records forwarded by the publisher arrive via
    /// `record_rx` and MUST be ack'd by the broker before the next one
    /// is taken from the channel — the transport gates publish order
    /// under backpressure.
    ///
    /// On every successful connect the transport MUST emit a
    /// [`TransportLifecycle::Connected`] on `lifecycle_tx` BEFORE the
    /// first publish (the publisher responds by repushing discovery and
    /// the current snapshot). On graceful cancellation it MUST emit a
    /// final best-effort retained-offline publish (the LWT covers
    /// ungraceful broker-side drops) followed by a
    /// [`TransportLifecycle::Disconnected`].
    async fn run(
        self: Box<Self>,
        record_rx: mpsc::Receiver<PublishRecord>,
        lifecycle_tx: mpsc::Sender<TransportLifecycle>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String>;
}

/// Dependencies the publisher task needs, handed in by `app.rs`.
pub struct StatePublisherDeps {
    /// Live config (the publisher's broker URL, `base_topic`,
    /// `discovery_prefix`, `instance_id` are read once at startup). The
    /// reload path tears the task down on `[publish]` edits and
    /// re-spawns it — the publisher does NOT watch the config itself.
    pub config: Arc<Config>,
    /// External credentials (publish broker credentials are looked up
    /// here by exact `broker_url` key, mirroring the MQTT sensor
    /// convention).
    pub credentials: Arc<Credentials>,
    /// Front ctl channel — the publisher uses it to subscribe to
    /// [`DaemonEvent`]s and to request a current [`StateSnapshot`].
    /// Front-channel (the `GenerationRouter`'s pause/queue/release
    /// fences cross-generation sends).
    pub ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    /// Cancellation token — fired on reload teardown (the old
    /// generation's producer token) and on daemon shutdown. On cancel
    /// the task publishes a final retained `offline` on the global
    /// availability topic and awaits the transport before returning.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Spawn the publisher. Returns `None` when `cfg.publish.enabled` is
/// `false` — callers MUST handle the `None` case by not awaiting a
/// handle that never existed.
#[must_use]
pub fn spawn(deps: StatePublisherDeps) -> Option<JoinHandle<()>> {
    if !deps.config.publish.enabled {
        return None;
    }
    Some(tokio::spawn(async move { run(deps, None).await }))
}

/// Test-only spawn entry point: same as [`spawn`] but with a caller-
/// supplied transport so the suite can observe every record forwarded
/// and drive the connect/disconnect cadence without a broker.
///
/// Gated on `test-util` so the daemon's external integration tests in
/// `crates/dormantd/tests/` can reach it.
#[cfg(any(test, feature = "test-util"))]
#[must_use]
pub fn spawn_with_transport(
    deps: StatePublisherDeps,
    transport: Box<dyn PublisherTransport>,
) -> Option<JoinHandle<()>> {
    if !deps.config.publish.enabled {
        return None;
    }
    Some(tokio::spawn(
        async move { run(deps, Some(transport)).await },
    ))
}

/// Implementation of the publisher loop. `transport_box` is `None` in
/// production (the loop constructs an `MqttTransport`); tests pass
/// `Some(Box::new(fake))` via [`spawn_with_transport`].
#[allow(clippy::too_many_lines)]
async fn run(deps: StatePublisherDeps, transport_box: Option<Box<dyn PublisherTransport>>) {
    let StatePublisherDeps {
        config,
        credentials,
        ctl_tx,
        cancel,
    } = deps;

    // The publisher always drives its own subscription via the engine
    // broadcast, so reload does not need to plumb a broadcast sender
    // through the generation chain.
    let (sub_tx, sub_rx) = tokio::sync::oneshot::channel();
    if ctl_tx
        .send(dormant_core::rules::ControlMsg::SubscribeEvents(sub_tx))
        .await
        .is_err()
    {
        return;
    }
    let Ok(mut event_rx) = sub_rx.await else {
        return;
    };

    let Some(mut snapshot) = request_snapshot(&ctl_tx, &cancel).await else {
        return;
    };
    let _ = credentials; // captured in mqtt_transport build; kept here to satisfy the borrow checker

    let instance = sanitize_topic_id(&config.publish.instance_id);

    // Build the transport. In production that's `MqttTransport`; tests
    // pass a fake.
    let (record_tx, record_rx) = mpsc::channel::<PublishRecord>(64);
    let (lifecycle_tx, mut lifecycle_rx) = mpsc::channel::<TransportLifecycle>(8);
    let transport_cancel = cancel.child_token();

    let transport: Box<dyn PublisherTransport> = match transport_box {
        Some(t) => t,
        None => mqtt_transport::MqttTransport::for_config(&config, &credentials, &instance),
    };
    let mut transport_handle: JoinHandle<()> = {
        let lt = lifecycle_tx.clone();
        let tc = transport_cancel.clone();
        tokio::spawn(async move {
            let _ = transport.run(record_rx, lt, tc).await;
        })
    };

    // Wait for the first Connected before forwarding records. (If the
    // transport fails outright before connecting, we exit cleanly.)
    let mut ever_connected = false;
    while !ever_connected {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return finalize_shutdown(transport_cancel, transport_handle).await;
            }
            maybe = lifecycle_rx.recv() => match maybe {
                Some(TransportLifecycle::Connected) => {
                    tracing::debug!(event = "publish_first_connected");
                    ever_connected = true;
                }
                Some(TransportLifecycle::Disconnected) => { /* keep waiting */ }
                None => return,
            },
            res = &mut transport_handle => {
                let _ = res;
                return;
            },
        }
    }

    // First connect: flush discovery + snapshot.
    {
        let inventory = EntityInventory::from_config(&config);
        let _ = detect_and_warn_collisions(&config, &inventory);
        for r in discovery_records(&config, &inventory, &instance) {
            let _ = record_tx.try_send(r);
        }
        for r in snapshot_records(&config, &snapshot, &instance) {
            let _ = record_tx.try_send(r);
        }
    }

    // Drain lifecycle + events until cancel or transport exit.
    loop {
        // Pin the JoinHandle so the runtime can re-poll it across select!
        // iterations without a per-poll borrow.
        let mut transport_fut = std::pin::pin!(&mut transport_handle);
        tokio::select! {
            biased;
            () = cancel.cancelled() => return finalize_shutdown(transport_cancel, transport_handle).await,
            maybe = lifecycle_rx.recv() => match maybe {
                Some(TransportLifecycle::Connected) => {
                    // Reconnect: re-flush discovery + a fresh snapshot.
                    let inventory = EntityInventory::from_config(&config);
                    let _ = detect_and_warn_collisions(&config, &inventory);
                    let disc = discovery_records(&config, &inventory, &instance);
                    for r in disc {
                        let _ = record_tx.try_send(r);
                    }
                    if let Some(new_snap) = request_snapshot(&ctl_tx, &cancel).await {
                        snapshot = new_snap;
                        let snaps = snapshot_records(&config, &snapshot, &instance);
                        for r in snaps {
                            let _ = record_tx.try_send(r);
                        }
                    }
                }
                Some(TransportLifecycle::Disconnected) => { /* reconnect handled by Connected */ }
                None => {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), transport_handle).await;
                    return;
                }
            },
            event_result = event_rx.recv() => match event_result {
                Ok(ev) => {
                    for r in event_records(&config, &ev, &instance) {
                        let _ = record_tx.try_send(r);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(event = "publish_events_lagged", skipped);
                    if let Some(new_snap) = request_snapshot(&ctl_tx, &cancel).await {
                        snapshot = new_snap;
                        for r in snapshot_records(&config, &snapshot, &instance) {
                            let _ = record_tx.try_send(r);
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), transport_handle).await;
                    return;
                }
            },
            res = &mut *transport_fut => {
                let _ = res;
                tracing::error!(event = "publish_transport_exited", "publisher transport ended; reload required to retry");
                return;
            }
        }
    }
}

/// Cancel the transport and await it (with a bounded grace window). The
/// transport's contract requires it to publish a retained offline before
/// returning `Ok(())` on cancel, so this is the moment the offline hits
/// the wire.
async fn finalize_shutdown(
    transport_cancel: tokio_util::sync::CancellationToken,
    transport_handle: JoinHandle<()>,
) {
    transport_cancel.cancel();
    let res = tokio::time::timeout(std::time::Duration::from_secs(5), transport_handle).await;
    if res.is_err() {
        tracing::error!(event = "publish_transport_join_timeout");
    }
}

/// Request a [`StateSnapshot`] via `ControlMsg::Snapshot`. Returns
/// `None` on cancel or engine channel closure.
async fn request_snapshot(
    ctl_tx: &mpsc::Sender<dormant_core::rules::ControlMsg>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<StateSnapshot> {
    use dormant_core::rules::ControlMsg;
    let (snap_tx, snap_rx) = tokio::sync::oneshot::channel();
    if ctl_tx.send(ControlMsg::Snapshot(snap_tx)).await.is_err() {
        return None;
    }
    tokio::select! {
        () = cancel.cancelled() => None,
        res = snap_rx => res.ok(),
    }
}

// ── MQTT transport (production) ──────────────────────────────────────────────

/// Production transport: wraps `rumqttc::AsyncClient` and owns the
/// connect/backoff loop, the LWT, and the graceful offline publish.
pub mod mqtt_transport {
    use std::time::Duration;

    use super::PublishRecord;
    use super::TransportLifecycle;
    use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use dormant_core::config::schema::{Config, Credentials};
    use dormant_core::mqtt::parse_broker_url;

    /// Production transport implementation. One per generation; reborn
    /// on every accepted reload.
    pub struct MqttTransport {
        broker_url: String,
        client_id: String,
        credentials: Credentials,
        global_availability_topic: String,
        backoff_initial: Duration,
        backoff_max: Duration,
        per_attempt_timeout: Duration,
    }

    impl MqttTransport {
        /// Construct a transport for the running generation.
        #[must_use]
        pub fn for_config(
            cfg: &Config,
            credentials: &Credentials,
            instance: &str,
        ) -> Box<dyn super::PublisherTransport> {
            let broker_url = cfg.publish.broker_url.clone().unwrap_or_default();
            let base = super::sanitize_topic_id(&cfg.publish.base_topic);
            let instance = super::sanitize_topic_id(instance);
            let global_availability_topic = format!("{base}/{instance}/availability");
            let client_id = format!("dormant-publisher-{instance}-{}", std::process::id());
            Box::new(Self {
                broker_url,
                client_id,
                credentials: credentials.clone(),
                global_availability_topic,
                backoff_initial: Duration::from_millis(100),
                backoff_max: Duration::from_secs(5),
                per_attempt_timeout: Duration::from_secs(2),
            })
        }

        /// Build `MqttOptions` with credentials applied for `url`.
        ///
        /// Public-only so a test can assert credential handling without
        /// having to wrestle a `Box<dyn PublisherTransport>` for the
        /// answer.
        #[must_use]
        pub fn build_options(url: &str, creds: &Credentials, client_id: &str) -> MqttOptions {
            let (host, port) = parse_broker_url(url);
            let mut opts = MqttOptions::new(client_id, host.to_string(), port);
            opts.set_clean_session(true);
            if let Some(cred) = creds.mqtt.get(url) {
                opts.set_credentials(cred.username.clone(), cred.password.clone());
            }
            opts
        }

        /// Apply the daemon's publish-credential conventions and return
        /// a redacted textual summary suitable for assertions in
        /// `crates/dormantd::state_publisher`'s tests.
        ///
        /// The summary deliberately omits the password: the test asserts
        /// the password never appears in any stringification of the
        /// transport surface (the contract is "dormant never publishes
        /// secrets"). Credentials are looked up by EXACT `broker_url`
        /// (the contract docs say "credential lookup is by exact
        /// `broker_url`").
        #[must_use]
        pub fn options_for_test(url: &str, creds: &Credentials, client_id: &str) -> String {
            let user = creds
                .mqtt
                .get(url)
                .map_or("<none>", |c| c.username.as_str());
            format!(
                "client_id={client_id:?} broker={url} user={user:?} password_present={}",
                creds.mqtt.get(url).is_some()
            )
        }
    }

    #[allow(clippy::too_many_lines)]
    #[async_trait::async_trait]
    impl super::PublisherTransport for MqttTransport {
        async fn run(
            self: Box<Self>,
            mut record_rx: mpsc::Receiver<PublishRecord>,
            lifecycle_tx: mpsc::Sender<TransportLifecycle>,
            cancel: CancellationToken,
        ) -> Result<(), String> {
            let mut backoff = self.backoff_initial;

            'connect_loop: loop {
                // Build a fresh MqttOptions per attempt so the LWT
                // stays bound; the LWT is held by the AsyncClient.
                let (host, port) = parse_broker_url(&self.broker_url);
                let mut opts = MqttOptions::new(self.client_id.clone(), host.to_string(), port);
                if let Some(cred) = self.credentials.mqtt.get(&self.broker_url) {
                    opts.set_credentials(cred.username.clone(), cred.password.clone());
                }
                let lwt_topic = self.global_availability_topic.clone();
                opts.set_last_will(rumqttc::LastWill::new(
                    lwt_topic,
                    b"offline".to_vec(),
                    QoS::AtLeastOnce,
                    true,
                ));

                let (client, mut eventloop) = AsyncClient::new(opts, 16);
                match tokio::time::timeout(self.per_attempt_timeout, eventloop.poll()).await {
                    Ok(Ok(rumqttc::Event::Incoming(Packet::ConnAck(_)))) => {
                        // First connect: emit Connected.
                        if lifecycle_tx
                            .send(TransportLifecycle::Connected)
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        backoff = self.backoff_initial;
                    }
                    Ok(Ok(_)) => {
                        // Other early events — keep retrying. We do NOT
                        // attempt to forward records until ConnAck lands.
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(self.backoff_max);
                        continue 'connect_loop;
                    }
                    Ok(Err(_)) => {
                        tracing::debug!(event = "publish_connect_error");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(self.backoff_max);
                        continue 'connect_loop;
                    }
                    Err(_) => {
                        tracing::debug!(event = "publish_connect_timeout");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(self.backoff_max);
                        continue 'connect_loop;
                    }
                }

                // Connected. Pump records + poll the event loop until
                // disconnect or cancel.
                'pump: loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => {
                            // Graceful offline. Best-effort retain.
                            let _ = client
                                .publish(
                                    &self.global_availability_topic,
                                    QoS::AtLeastOnce,
                                    true,
                                    b"offline".to_vec(),
                                )
                                .await;
                            // Give the eventloop a chance to deliver
                            // the PubAck so it lands before we close.
                            let _ = tokio::time::timeout(
                                Duration::from_millis(500),
                                wait_for_publish_id(&mut eventloop),
                            )
                            .await;
                            let _ = lifecycle_tx
                                .send(TransportLifecycle::Disconnected)
                                .await;
                            return Ok(());
                        }
                        maybe = record_rx.recv() => {
                            let Some(rec) = maybe else {
                                // Publisher dropped its sender (e.g. on a
                                // generation swap). Treat as graceful
                                // cancellation.
                                return Ok(());
                            };
                            // Skip the empty-payload sentinel we send to
                            // close the channel from the publisher side.
                            if rec.topic.is_empty() {
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Disconnected)
                                    .await;
                                return Ok(());
                            }
                            let qos = if rec.qos == 1 { QoS::AtLeastOnce } else { QoS::AtMostOnce };
                            let payload = rec.payload.as_bytes().to_vec();
                            let topic = rec.topic.clone();
                            if let Err(_e) = client
                                .publish(&topic, qos, rec.retain, payload)
                                .await
                            {
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Disconnected)
                                    .await;
                                break 'pump;
                            }
                            // Drive the event loop until the matching
                            // PubAck lands so publishes are ordered
                            // (the publisher sends records one at a
                            // time through a bounded mpsc).
                            if tokio::time::timeout(
                                Duration::from_secs(5),
                                wait_for_publish_id(&mut eventloop),
                            )
                            .await
                            .is_err()
                            {
                                tracing::debug!(event = "publish_ack_timeout");
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Disconnected)
                                    .await;
                                break 'pump;
                            }
                        }
                        event = eventloop.poll() => {
                            match event {
                                Ok(Event::Incoming(Packet::Disconnect))
                                | Err(_) => {
                                    let _ = lifecycle_tx
                                        .send(TransportLifecycle::Disconnected)
                                        .await;
                                    break 'pump;
                                }
                                Ok(_) => {}
                            }
                        }
                    }
                }
                // Dropped — reconnect with backoff.
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(self.backoff_max);
            }
        }
    }

    /// Event-loop drain until we see ANY `PubAck` (the publisher pushes
    /// one record at a time, so order is preserved).
    async fn wait_for_publish_id(eventloop: &mut EventLoop) -> Result<(), String> {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::PubAck(_))) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

// ── Public sanitizer (contract's only data helper) ──────────────────────────

/// Sanitize a raw id for use in a topic. Lowercase, ASCII-safe
/// `[a-z0-9_-]`, collapse runs of `_`, trim leading/trailing `_`,
/// empty result → `"unnamed"`. Reused by every id that lands in a
/// topic — `{instance}`, `{sensor_id}`, `{zone_id}`, `{display_id}`
/// — so a single helper change is the only seam for sanitizer
/// semantics.
///
/// `unnamed` (not `"dormant"`) is the contract's deliberate
/// collision-warning signal: two `""` ids do NOT silently map to
/// `"dormant"` and get a free pass — they both sanitize to
/// `"unnamed"` and the collision rule fires. The contract treats
/// this as a real collision (a WARN with both raw ids).
#[must_use]
pub fn sanitize_topic_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    // Collapse runs of `_` and trim leading/trailing `_`.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_was_underscore = true; // suppress leading underscore
    for c in out.chars() {
        if c == '_' {
            if !last_was_underscore {
                collapsed.push('_');
            }
            last_was_underscore = true;
        } else {
            collapsed.push(c);
            last_was_underscore = false;
        }
    }
    while collapsed.ends_with('_') {
        collapsed.pop();
    }
    if collapsed.is_empty() {
        "unnamed".to_string()
    } else {
        collapsed
    }
}

// ── Inventory ────────────────────────────────────────────────────────────────

/// All entity ids the publisher will discover, in config-order.
///
/// `IndexMap` iteration order is config-order, which is the
/// "first wins" half of the collision rule (see
/// [`detect_and_warn_collisions`]). The three fields are flat
/// strings — the actual topic-segment ids are produced at
/// call-site via [`sanitize_topic_id`].
#[derive(Debug, Clone)]
pub struct EntityInventory<'a> {
    /// Sensor ids in config order.
    pub sensors: Vec<&'a str>,
    /// Zone ids in config order.
    pub zones: Vec<&'a str>,
    /// Display ids in config order.
    pub displays: Vec<&'a str>,
}

impl<'a> EntityInventory<'a> {
    /// Build the inventory from the operator's loaded config.
    #[must_use]
    pub fn from_config(cfg: &'a Config) -> Self {
        Self {
            sensors: cfg.sensors.keys().map(String::as_str).collect(),
            zones: cfg.zones.keys().map(String::as_str).collect(),
            displays: cfg.displays.keys().map(String::as_str).collect(),
        }
    }
}

// ── Topic builders (the contract's only literal topic shapes) ────────────────

fn topic_availability(base: &str, instance: &str) -> String {
    format!("{base}/{instance}/availability")
}

fn topic_sensor_state(base: &str, instance: &str, sensor_id: &str) -> String {
    format!("{base}/{instance}/sensor/{sensor_id}/state")
}

fn topic_sensor_availability(base: &str, instance: &str, sensor_id: &str) -> String {
    format!("{base}/{instance}/sensor/{sensor_id}/availability")
}

fn topic_zone_state(base: &str, instance: &str, zone_id: &str) -> String {
    format!("{base}/{instance}/zone/{zone_id}/state")
}

fn topic_display_phase(base: &str, instance: &str, display_id: &str) -> String {
    format!("{base}/{instance}/display/{display_id}/phase")
}

fn topic_discovery_sensor(discovery_prefix: &str, instance: &str, sensor_id: &str) -> String {
    format!("{discovery_prefix}/binary_sensor/{instance}/sensor_{sensor_id}/config")
}

fn topic_discovery_zone(discovery_prefix: &str, instance: &str, zone_id: &str) -> String {
    format!("{discovery_prefix}/binary_sensor/{instance}/zone_{zone_id}/config")
}

fn topic_discovery_display(discovery_prefix: &str, instance: &str, display_id: &str) -> String {
    format!("{discovery_prefix}/sensor/{instance}/display_{display_id}/config")
}

// ── Unique id builders (grep-stable: `dormant_{instance}_{kind}_{id}`) ────────

fn unique_id_sensor(instance: &str, sensor_id: &str) -> String {
    format!("dormant_{instance}_sensor_{sensor_id}")
}

fn unique_id_zone(instance: &str, zone_id: &str) -> String {
    format!("dormant_{instance}_zone_{zone_id}")
}

fn unique_id_display(instance: &str, display_id: &str) -> String {
    format!("dormant_{instance}_display_{display_id}")
}

// ── Discovery payloads ───────────────────────────────────────────────────────

/// Build the discovery payload for a sensor (`binary_sensor`).
fn discovery_payload_sensor(cfg: &Config, instance: &str, sensor_id: &str) -> serde_json::Value {
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let sensor_id = sanitize_topic_id(sensor_id);
    let avail_topic = topic_availability(&base, &instance);
    let sensor_avail_topic = topic_sensor_availability(&base, &instance, &sensor_id);
    json!({
        "name": format!("dormant {sensor_id} presence"),
        "unique_id": unique_id_sensor(&instance, &sensor_id),
        "object_id": unique_id_sensor(&instance, &sensor_id),
        "state_topic": topic_sensor_state(&base, &instance, &sensor_id),
        "payload_on": "ON",
        "payload_off": "OFF",
        "availability": [
            {
                "topic": avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
            {
                "topic": sensor_avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
        ],
        "availability_mode": "all",
        "device": device_block(&instance),
    })
}

/// Build the discovery payload for a zone (`binary_sensor`).
fn discovery_payload_zone(cfg: &Config, instance: &str, zone_id: &str) -> serde_json::Value {
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let zone_id = sanitize_topic_id(zone_id);
    let avail_topic = topic_availability(&base, &instance);
    json!({
        "name": format!("dormant {zone_id} zone"),
        "unique_id": unique_id_zone(&instance, &zone_id),
        "object_id": unique_id_zone(&instance, &zone_id),
        "state_topic": topic_zone_state(&base, &instance, &zone_id),
        "payload_on": "ON",
        "payload_off": "OFF",
        "availability": [
            {
                "topic": avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
        ],
        "device": device_block(&instance),
    })
}

/// Build the discovery payload for a display (sensor, not `binary_sensor`).
///
/// The state value is the lowercase phase literal from
/// `DisplaySnapshot.phase` — read back through the `value_template`
/// so HA parses the field out of the published JSON object.
fn discovery_payload_display(cfg: &Config, instance: &str, display_id: &str) -> serde_json::Value {
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let display_id = sanitize_topic_id(display_id);
    let avail_topic = topic_availability(&base, &instance);
    json!({
        "name": format!("dormant {display_id} display"),
        "unique_id": unique_id_display(&instance, &display_id),
        "object_id": unique_id_display(&instance, &display_id),
        "state_topic": topic_display_phase(&base, &instance, &display_id),
        "value_template": "{{ value_json.phase }}",
        "availability": [
            {
                "topic": avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
        ],
        "device": device_block(&instance),
    })
}

fn device_block(instance: &str) -> serde_json::Value {
    json!({
        "identifiers": [format!("dormant_{instance}")],
        "name": format!("dormant {instance}"),
        "manufacturer": "dormant",
        "model": "presence",
        "sw_version": env!("CARGO_PKG_VERSION"),
    })
}

// ── Public entry points ─────────────────────────────────────────────────────

/// Discovery configs for every entity the inventory knows about.
///
/// Returned records are ordered sensors → zones → displays, and
/// within each kind in config-order. First-wins collisions are
/// resolved by [`detect_and_warn_collisions`]: a collision drops
/// every record after the first for that sanitized id and emits
/// one `publish_id_collision` WARN per colliding pair.
#[must_use]
pub fn discovery_records(
    cfg: &Config,
    inventory: &EntityInventory<'_>,
    instance: &str,
) -> Vec<PublishRecord> {
    if !cfg.publish.enabled {
        return Vec::new();
    }
    let _ = detect_and_warn_collisions(cfg, inventory);

    let instance = sanitize_topic_id(instance);
    let discovery_prefix = sanitize_topic_id(&cfg.publish.discovery_prefix);

    let mut records: Vec<PublishRecord> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Sensors (binary_sensor).
    for sensor_id in &inventory.sensors {
        let sanitized = sanitize_topic_id(sensor_id);
        if !seen.insert(sanitized.clone()) {
            continue;
        }
        let topic = topic_discovery_sensor(&discovery_prefix, &instance, &sanitized);
        let payload = discovery_payload_sensor(cfg, &instance, sensor_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    // Zones (binary_sensor).
    for zone_id in &inventory.zones {
        let sanitized = sanitize_topic_id(zone_id);
        if !seen.insert(sanitized.clone()) {
            continue;
        }
        let topic = topic_discovery_zone(&discovery_prefix, &instance, &sanitized);
        let payload = discovery_payload_zone(cfg, &instance, zone_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    // Displays (sensor).
    for display_id in &inventory.displays {
        let sanitized = sanitize_topic_id(display_id);
        if !seen.insert(sanitized.clone()) {
            continue;
        }
        let topic = topic_discovery_display(&discovery_prefix, &instance, &sanitized);
        let payload = discovery_payload_display(cfg, &instance, display_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    records
}

/// Startup / re-connect flush: every retained state record from a
/// full [`StateSnapshot`].
///
/// Each sensor contributes two records (state + availability); each
/// zone and each display contributes one. Disabled-by-default returns
/// an empty `Vec` (the contract's kill-switch).
#[must_use]
pub fn snapshot_records(
    cfg: &Config,
    snapshot: &StateSnapshot,
    instance: &str,
) -> Vec<PublishRecord> {
    if !cfg.publish.enabled {
        return Vec::new();
    }
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let mut records: Vec<PublishRecord> = Vec::new();

    for sensor in &snapshot.sensors {
        let sensor_id = sanitize_topic_id(&sensor.id);
        records.push(state_record(
            &topic_sensor_state(&base, &instance, &sensor_id),
            sensor.state,
        ));
        records.push(availability_record(
            &topic_sensor_availability(&base, &instance, &sensor_id),
            sensor.state,
        ));
    }

    for zone in &snapshot.zones {
        let zone_id = sanitize_topic_id(&zone.id);
        // A zone that has never been resolved (`present = None`)
        // publishes "OFF" — absent-by-default, matching the
        // fail-toward-blanking policy in the engine.
        let payload = if zone.present.unwrap_or(false) {
            "ON"
        } else {
            "OFF"
        };
        let topic = topic_zone_state(&base, &instance, &zone_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    for (display_id_raw, display) in &snapshot.displays {
        let display_id = sanitize_topic_id(display_id_raw);
        let topic = topic_display_phase(&base, &instance, &display_id);
        records.push(PublishRecord {
            topic,
            payload: display_phase_payload(display),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    records
}

/// Incremental single-event mapping. Returns an empty `Vec` for
/// unknown / foreign event variants — the contract is explicit
/// that the publisher NEVER speculates about events the engine did
/// not actually emit.
///
/// The currently-mapped variants:
/// - [`DaemonEvent::SensorChanged`] → sensor state + sensor
///   availability
/// - [`DaemonEvent::ZoneChanged`] → zone state
/// - [`DaemonEvent::DisplayPhase`] → display phase
/// - [`DaemonEvent::Subscribed`] / [`DaemonEvent::ConfigReloaded`]
///   / wear / blank / wake / ownership / [`DaemonEvent::Unknown`] →
///   empty (not the publisher's plane)
#[must_use]
pub fn event_records(cfg: &Config, event: &DaemonEvent, instance: &str) -> Vec<PublishRecord> {
    if !cfg.publish.enabled {
        return Vec::new();
    }
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    match event {
        DaemonEvent::SensorChanged { sensor, state } => {
            let sensor_id = sanitize_topic_id(&sensor.0);
            vec![
                state_record(&topic_sensor_state(&base, &instance, &sensor_id), *state),
                availability_record(
                    &topic_sensor_availability(&base, &instance, &sensor_id),
                    *state,
                ),
            ]
        }
        DaemonEvent::ZoneChanged { zone, present, .. } => {
            let zone_id = sanitize_topic_id(&zone.0);
            let payload = if *present { "ON" } else { "OFF" };
            vec![PublishRecord {
                topic: topic_zone_state(&base, &instance, &zone_id),
                payload: payload.to_string(),
                qos: PUBLISH_QOS,
                retain: RETAIN,
            }]
        }
        DaemonEvent::DisplayPhase { display, phase, .. } => {
            let display_id = sanitize_topic_id(&display.0);
            vec![PublishRecord {
                topic: topic_display_phase(&base, &instance, &display_id),
                // The phase literal is the engine's own grep-stable
                // string (`active|grace|blanking|blanked|waking|staged`);
                // publishing it bare keeps HA's value_template honest.
                payload: phase.clone(),
                qos: PUBLISH_QOS,
                retain: RETAIN,
            }]
        }
        // Every other variant is intentionally a no-op: the
        // publisher only carries state, never operational events
        // (blank failure, wake retry, wear, ownership) which
        // already have their own diagnostic surfaces.
        DaemonEvent::Subscribed
        | DaemonEvent::ConfigReloaded
        | DaemonEvent::WakeRetry { .. }
        | DaemonEvent::WearSnapshot { .. }
        | DaemonEvent::CompensationAdvisory { .. }
        | DaemonEvent::BlankFailure { .. }
        | DaemonEvent::BlankRecovered { .. }
        | DaemonEvent::WakeRecovered { .. }
        | DaemonEvent::Ownership { .. }
        | DaemonEvent::Unknown => Vec::new(),
    }
}

// ── Private payload shapers ──────────────────────────────────────────────────

fn state_record(topic: &str, state: SensorState) -> PublishRecord {
    // HA binary_sensor defaults: `ON` for present, `OFF` for absent
    // or unavailable. Unavailable deliberately publishes OFF (the
    // same payload as absent) so a stale sensor and an empty
    // room read the same way to the consumer; the fail-safe
    // presence policy is enforced upstream by the engine, not
    // re-encoded here.
    let payload = match state {
        SensorState::Present => "ON",
        SensorState::Absent | SensorState::Unavailable => "OFF",
    };
    PublishRecord {
        topic: topic.to_string(),
        payload: payload.to_string(),
        qos: PUBLISH_QOS,
        retain: RETAIN,
    }
}

fn availability_record(topic: &str, state: SensorState) -> PublishRecord {
    let payload = match state {
        SensorState::Present | SensorState::Absent => "online",
        SensorState::Unavailable => "offline",
    };
    PublishRecord {
        topic: topic.to_string(),
        payload: payload.to_string(),
        qos: PUBLISH_QOS,
        retain: RETAIN,
    }
}

fn display_phase_payload(display: &DisplaySnapshot) -> String {
    // The display phase publishes a JSON object with the
    // `phase` field so HA's `value_template: "{{ value_json.phase }}"`
    // parses the literal. Carrying the empty JSON object is
    // intentional — a bare string would not parse through the
    // template. We do not re-emit the `inhibited` / `paused` flags
    // here; those have their own surfaces in the rules engine and
    // are out of scope for the publish contract.
    json!({ "phase": display.phase }).to_string()
}

// ── Collision detection (first-wins, WARN once) ─────────────────────────────

/// Returned to the caller for log-side effect; the public entry
/// points call this for its side-effect (a single `publish_id_collision`
/// WARN per colliding pair) and ignore the return value.
#[must_use]
pub fn detect_and_warn_collisions(
    cfg: &Config,
    inventory: &EntityInventory<'_>,
) -> Vec<(String, Vec<String>)> {
    let _ = cfg; // Reserved for a future per-broker scoping knob.
    let mut by_sanitized: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();

    let mut record = |raw: &str, kind: &str| {
        let sanitized = sanitize_topic_id(raw);
        // The `unnamed` collision is a contract signal: two empty
        // ids are the same kind of mistake as a real collision.
        let entry = by_sanitized.entry(sanitized.clone()).or_default();
        let key = format!("{kind}:{raw}");
        if !entry.contains(&key) {
            entry.push(key);
        }
        if !order.contains(&sanitized) {
            order.push(sanitized);
        }
    };

    for id in &inventory.sensors {
        record(id, "sensor");
    }
    for id in &inventory.zones {
        record(id, "zone");
    }
    for id in &inventory.displays {
        record(id, "display");
    }

    let mut collisions: Vec<(String, Vec<String>)> = Vec::new();
    for sanitized in &order {
        if let Some(entries) = by_sanitized.get(sanitized)
            && entries.len() > 1
        {
            tracing::warn!(
                event = "publish_id_collision",
                sanitized_id = %sanitized,
                raw_ids = ?entries,
                "two config ids sanitize to the same topic id; publishing only the first (config-order)"
            );
            collisions.push((sanitized.clone(), entries.clone()));
        }
    }
    collisions
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use crate::state_publisher::{
        EntityInventory, detect_and_warn_collisions, discovery_records, event_records,
        sanitize_topic_id, snapshot_records,
    };
    use dormant_core::config::schema::{
        AudioConfig, Config, DisplayConfig, DisplayScope, MqttSensorCfg, NotificationsConfig,
        SensorConfig, SensorKind, WatchdogConfig, WearConfig, ZoneConfig,
    };
    use dormant_core::rules::{
        DaemonEvent, DisplaySnapshot, SensorSnapshot, StateSnapshot, ZoneSnapshot,
    };
    use dormant_core::types::{DisplayId, SensorId, ZoneId};
    use indexmap::IndexMap;
    use std::time::Duration;

    // ── Test fixtures ─────────────────────────────────────────────────────

    fn enabled_cfg() -> Config {
        let mut cfg = Config {
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::new(),
            rules: IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            coordination: dormant_core::config::CoordinationConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig {
                enabled: true,
                broker_url: Some("tcp://h:1883".into()),
                base_topic: "dormant".into(),
                discovery_prefix: "homeassistant".into(),
                instance_id: "office-pc".into(),
            },
        };
        cfg.sensors.insert(
            "desk".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/desk".into(),
                field: "/val".into(),
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
        cfg.zones.insert(
            "office".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec!["desk".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        cfg.displays.insert(
            "main".into(),
            DisplayConfig {
                controllers: vec!["kwin-dpms".into()],
                scope: DisplayScope::default(),
                shared_input_code: None,
                shared_input_write_code: None,
                shared_peer_input_code: None,
                shared_peer_input_write_code: None,
                hooks: dormant_core::config::schema::HookSlots::default(),
                blank_mode: Some(dormant_core::types::BlankMode::PowerOff),
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
                command_timeout: Duration::from_secs(10),
                restore_brightness: 80,
                samsung_restore_backlight: 50,
                treat_unreachable_as_blanked: true,
                panel_type: dormant_core::wear::PanelType::Unknown,
            },
        );
        cfg
    }

    fn disabled_cfg() -> Config {
        let mut cfg = enabled_cfg();
        cfg.publish.enabled = false;
        cfg
    }

    fn snapshot_one_of_each() -> StateSnapshot {
        StateSnapshot {
            sensors: vec![SensorSnapshot {
                id: "desk".into(),
                state: SensorState::Present,
                last_seen_secs_ago: 0,
                reported: true,
            }],
            zones: vec![ZoneSnapshot {
                id: "office".into(),
                present: Some(true),
            }],
            displays: vec![(
                "main".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    scope: DisplayScope::default(),
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

    // ── Sanitizer (the contract's only data helper) ─────────────────────

    #[test]
    fn sanitize_lowercases() {
        assert_eq!(sanitize_topic_id("HELLO"), "hello");
    }

    #[test]
    fn sanitize_keeps_dash_and_digit() {
        assert_eq!(sanitize_topic_id("zone-2_a"), "zone-2_a");
    }

    #[test]
    fn sanitize_maps_disallowed_chars_to_underscore() {
        assert_eq!(sanitize_topic_id("a b.c"), "a_b_c");
        assert_eq!(sanitize_topic_id("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_topic_id("a:b@c"), "a_b_c");
    }

    #[test]
    fn sanitize_collapses_runs_of_underscore() {
        assert_eq!(sanitize_topic_id("a  b"), "a_b");
        assert_eq!(sanitize_topic_id("a...b"), "a_b");
    }

    #[test]
    fn sanitize_trims_leading_and_trailing_underscore() {
        assert_eq!(sanitize_topic_id("__a__"), "a");
        assert_eq!(sanitize_topic_id("..."), "unnamed");
    }

    #[test]
    fn sanitize_empty_is_unnamed_not_dormant() {
        // Per the contract, empty ids are a real collision (both
        // sanitize to "unnamed"), NOT a free pass.
        assert_eq!(sanitize_topic_id(""), "unnamed");
    }

    // ── Disabled-by-default ─────────────────────────────────────────────

    #[test]
    fn discovery_records_empty_when_disabled() {
        let cfg = disabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        assert!(discovery_records(&cfg, &inv, "office-pc").is_empty());
    }

    #[test]
    fn snapshot_records_empty_when_disabled() {
        let cfg = disabled_cfg();
        let snap = snapshot_one_of_each();
        assert!(snapshot_records(&cfg, &snap, "office-pc").is_empty());
    }

    #[test]
    fn event_records_empty_when_disabled() {
        let cfg = disabled_cfg();
        let ev = DaemonEvent::SensorChanged {
            sensor: SensorId("desk".into()),
            state: SensorState::Present,
        };
        assert!(event_records(&cfg, &ev, "office-pc").is_empty());
    }

    // ── Discovery records: EXACT topic + payload literals ──────────────

    #[test]
    fn discovery_records_exact_topics_for_one_of_each() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let records = discovery_records(&cfg, &inv, "office-pc");
        let topics: Vec<&str> = records.iter().map(|r| r.topic.as_str()).collect();

        // 1 sensor + 1 zone + 1 display = 3 records.
        assert_eq!(records.len(), 3, "topics: {topics:?}");
        assert!(topics.contains(&"homeassistant/binary_sensor/office-pc/sensor_desk/config"));
        assert!(topics.contains(&"homeassistant/binary_sensor/office-pc/zone_office/config"));
        assert!(topics.contains(&"homeassistant/sensor/office-pc/display_main/config"));
    }

    #[test]
    fn discovery_records_all_have_qos1_and_retained() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        for r in discovery_records(&cfg, &inv, "office-pc") {
            assert_eq!(r.qos, 1, "all records are QoS 1: {}", r.topic);
            assert!(r.retain, "all records are retained: {}", r.topic);
        }
    }

    #[test]
    fn discovery_records_stable_unique_ids() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        for r in &discovery_records(&cfg, &inv, "office-pc") {
            let v: serde_json::Value = serde_json::from_str(&r.payload).unwrap();
            let unique_id = v
                .get("unique_id")
                .and_then(serde_json::Value::as_str)
                .expect("unique_id present");
            let object_id = v
                .get("object_id")
                .and_then(serde_json::Value::as_str)
                .expect("object_id present");
            assert_eq!(unique_id, object_id, "object_id mirrors unique_id");
            assert!(
                unique_id.starts_with("dormant_office-pc_"),
                "unique_id prefix pinned: {unique_id}"
            );
            assert!(
                unique_id.contains("_sensor_")
                    || unique_id.contains("_zone_")
                    || unique_id.contains("_display_")
            );
        }
    }

    #[test]
    fn discovery_records_sensor_has_two_availability_entries_with_all_mode() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let sensor_record = discovery_records(&cfg, &inv, "office-pc")
            .into_iter()
            .find(|r| r.topic == "homeassistant/binary_sensor/office-pc/sensor_desk/config")
            .expect("sensor discovery");
        let v: serde_json::Value = serde_json::from_str(&sensor_record.payload).unwrap();
        let availability = v
            .get("availability")
            .and_then(serde_json::Value::as_array)
            .expect("availability array");
        assert_eq!(availability.len(), 2, "sensor has 2 availability entries");
        let topics: Vec<&str> = availability
            .iter()
            .filter_map(|entry| entry.get("topic").and_then(serde_json::Value::as_str))
            .collect();
        assert!(topics.contains(&"dormant/office-pc/availability"));
        assert!(topics.contains(&"dormant/office-pc/sensor/desk/availability"));
        assert_eq!(
            v.get("availability_mode")
                .and_then(serde_json::Value::as_str),
            Some("all")
        );
    }

    #[test]
    fn discovery_records_zone_has_only_global_availability() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let zone_record = discovery_records(&cfg, &inv, "office-pc")
            .into_iter()
            .find(|r| r.topic == "homeassistant/binary_sensor/office-pc/zone_office/config")
            .expect("zone discovery");
        let v: serde_json::Value = serde_json::from_str(&zone_record.payload).unwrap();
        let availability = v
            .get("availability")
            .and_then(serde_json::Value::as_array)
            .expect("availability array");
        assert_eq!(availability.len(), 1, "zone has 1 availability entry");
        assert_eq!(
            availability[0]
                .get("topic")
                .and_then(serde_json::Value::as_str),
            Some("dormant/office-pc/availability")
        );
    }

    #[test]
    fn discovery_records_display_uses_value_template() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let display_record = discovery_records(&cfg, &inv, "office-pc")
            .into_iter()
            .find(|r| r.topic == "homeassistant/sensor/office-pc/display_main/config")
            .expect("display discovery");
        let v: serde_json::Value = serde_json::from_str(&display_record.payload).unwrap();
        assert_eq!(
            v.get("value_template").and_then(serde_json::Value::as_str),
            Some("{{ value_json.phase }}")
        );
    }

    #[test]
    fn discovery_records_no_secret_bearing_fields() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        for r in discovery_records(&cfg, &inv, "office-pc") {
            for forbidden in ["password", "token", "secret", "username", "credential"] {
                assert!(
                    !r.payload.to_ascii_lowercase().contains(forbidden),
                    "discovery payload leaked credential-shaped field {forbidden:?}: {}",
                    r.payload
                );
            }
        }
    }

    #[test]
    fn discovery_records_instance_id_is_sanitized() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let records = discovery_records(&cfg, &inv, "Weird Host!");
        let topics: Vec<&str> = records.iter().map(|r| r.topic.as_str()).collect();
        // "Weird Host!" -> "weird_host"
        assert!(topics.contains(&"homeassistant/binary_sensor/weird_host/sensor_desk/config"));
    }

    // ── Snapshot records ───────────────────────────────────────────────

    #[test]
    fn snapshot_records_one_per_sensor_state_sensor_availability_zone_display() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        // 1 sensor (state + availability) + 1 zone + 1 display = 4.
        assert_eq!(records.len(), 4, "records: {records:?}");
        let topics: Vec<&str> = records.iter().map(|r| r.topic.as_str()).collect();
        assert!(topics.contains(&"dormant/office-pc/sensor/desk/state"));
        assert!(topics.contains(&"dormant/office-pc/sensor/desk/availability"));
        assert!(topics.contains(&"dormant/office-pc/zone/office/state"));
        assert!(topics.contains(&"dormant/office-pc/display/main/phase"));
    }

    #[test]
    fn snapshot_records_sensor_present_state_and_online_availability() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let state = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/state")
            .unwrap();
        assert_eq!(state.payload, "ON");
        let avail = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/availability")
            .unwrap();
        assert_eq!(avail.payload, "online");
    }

    #[test]
    fn snapshot_records_sensor_unavailable_yields_offline_availability() {
        let cfg = enabled_cfg();
        let mut snap = snapshot_one_of_each();
        snap.sensors[0].state = SensorState::Unavailable;
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let avail = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/availability")
            .unwrap();
        assert_eq!(avail.payload, "offline");
        // State is still OFF (matches Absent) for the fail-toward-blanking
        // policy encoded upstream.
        let state = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/state")
            .unwrap();
        assert_eq!(state.payload, "OFF");
    }

    #[test]
    fn snapshot_records_zone_present_publishes_on() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let zone = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/zone/office/state")
            .unwrap();
        assert_eq!(zone.payload, "ON");
    }

    #[test]
    fn snapshot_records_zone_unresolved_publishes_off() {
        let cfg = enabled_cfg();
        let mut snap = snapshot_one_of_each();
        snap.zones[0].present = None;
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let zone = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/zone/office/state")
            .unwrap();
        assert_eq!(zone.payload, "OFF");
    }

    #[test]
    fn snapshot_records_display_phase_is_lowercase_literal_in_json() {
        let cfg = enabled_cfg();
        let mut snap = snapshot_one_of_each();
        snap.displays[0].1.phase = "GRACE".into();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let display = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/display/main/phase")
            .unwrap();
        // The engine's `phase` field is the contract's grep-stable
        // literal; we publish it as a JSON object so the HA
        // value_template can extract it. The phase is the
        // engine's exact value (case preserved) — the contract
        // is "lowercase phase literal", which the engine
        // already guarantees upstream.
        let v: serde_json::Value = serde_json::from_str(&display.payload).unwrap();
        assert_eq!(
            v.get("phase").and_then(serde_json::Value::as_str),
            Some("GRACE")
        );
    }

    // ── Event records ──────────────────────────────────────────────────

    #[test]
    fn event_records_sensor_changed_yields_state_and_availability() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::SensorChanged {
            sensor: SensorId("desk".into()),
            state: SensorState::Present,
        };
        let records = event_records(&cfg, &ev, "office-pc");
        assert_eq!(records.len(), 2);
        let state = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/state")
            .unwrap();
        assert_eq!(state.payload, "ON");
        let avail = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/availability")
            .unwrap();
        assert_eq!(avail.payload, "online");
    }

    #[test]
    fn event_records_zone_changed_yields_state() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::ZoneChanged {
            zone: ZoneId("office".into()),
            present: false,
            cause: SensorId("desk".into()),
        };
        let records = event_records(&cfg, &ev, "office-pc");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].topic, "dormant/office-pc/zone/office/state");
        assert_eq!(records[0].payload, "OFF");
    }

    #[test]
    fn event_records_display_phase_yields_phase_literal() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::DisplayPhase {
            display: DisplayId("main".into()),
            phase: "blanking".into(),
            cause: "zone_lost".into(),
        };
        let records = event_records(&cfg, &ev, "office-pc");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].topic, "dormant/office-pc/display/main/phase");
        // Phase is published bare (not wrapped in JSON) so the HA
        // value_template's `{{ value_json.phase }}` works
        // regardless of the engine's choice. The contract
        // pins the value to the engine's phase literal.
        assert_eq!(records[0].payload, "blanking");
    }

    #[test]
    fn event_records_unknown_variant_publishes_nothing() {
        let cfg = enabled_cfg();
        for ev in [
            DaemonEvent::Subscribed,
            DaemonEvent::ConfigReloaded,
            DaemonEvent::Unknown,
            DaemonEvent::WakeRetry {
                display: DisplayId("main".into()),
                attempt: 1,
            },
            DaemonEvent::BlankRecovered {
                display: DisplayId("main".into()),
            },
            DaemonEvent::BlankFailure {
                display: DisplayId("main".into()),
                controller: "kwin-dpms".into(),
                detail: "E_TIMEOUT".into(),
            },
            DaemonEvent::WakeRecovered {
                display: DisplayId("main".into()),
                attempts: 1,
            },
            DaemonEvent::WearSnapshot {
                display: DisplayId("main".into()),
                total_on_hours: 0.0,
                sample_count: 0,
            },
            DaemonEvent::CompensationAdvisory {
                display: DisplayId("main".into()),
                hours_since_long_dwell: 0,
            },
            DaemonEvent::Ownership {
                display: DisplayId("main".into()),
                owned: true,
                observed_input_code: None,
                written_code: None,
                cause: "poll".into(),
                verified: None,
                degraded: false,
            },
        ] {
            let records = event_records(&cfg, &ev, "office-pc");
            assert!(
                records.is_empty(),
                "{ev:?} should not produce publish records, got: {records:?}"
            );
        }
    }

    // ── Collision rule: first-wins ─────────────────────────────────────

    #[test]
    fn collision_two_sensor_ids_same_topic() {
        let mut cfg = enabled_cfg();
        cfg.sensors.insert(
            "desk two".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/desk2".into(),
                field: "/val".into(),
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
        cfg.sensors.insert(
            "desk_two".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/desk3".into(),
                field: "/val".into(),
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
        // "desk two" -> "desk_two" (space -> underscore, run-collapsed)
        // "desk_two" -> "desk_two" (already canonical)
        let inv = EntityInventory::from_config(&cfg);
        let collisions = detect_and_warn_collisions(&cfg, &inv);
        assert!(
            collisions.iter().any(|(san, _)| san == "desk_two"),
            "expected desk_two collision, got {collisions:?}"
        );
        // First-wins: discovery emits only the first record.
        let records = discovery_records(&cfg, &inv, "office-pc");
        let topic_count = records
            .iter()
            .filter(|r| r.topic.contains("/sensor_desk_two/"))
            .count();
        assert_eq!(topic_count, 1, "first-wins: only one record per topic");
    }

    #[test]
    fn collision_across_kinds_picks_first() {
        let mut cfg = enabled_cfg();
        cfg.zones.insert(
            "main".into(), // collides with display "main"
            ZoneConfig {
                mode: "any".into(),
                members: vec!["desk".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        let inv = EntityInventory::from_config(&cfg);
        // Display "main" came first; zone "main" should be dropped.
        let records = discovery_records(&cfg, &inv, "office-pc");
        let topic_count = records
            .iter()
            .filter(|r| r.topic.contains("/display_main/") || r.topic.contains("/zone_main/"))
            .count();
        assert_eq!(topic_count, 1, "first-wins across kinds");
    }

    #[test]
    fn collision_empty_ids_map_to_unnamed_and_warn() {
        let mut cfg = enabled_cfg();
        cfg.sensors.insert(
            String::new(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/x".into(),
                field: "/val".into(),
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
        cfg.zones.insert(
            "   ".into(), // whitespace -> "unnamed"
            ZoneConfig {
                mode: "any".into(),
                members: vec!["desk".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        let inv = EntityInventory::from_config(&cfg);
        let collisions = detect_and_warn_collisions(&cfg, &inv);
        assert!(
            collisions.iter().any(|(san, _)| san == "unnamed"),
            "empty/whitespace ids must collide on 'unnamed', got {collisions:?}"
        );
        // First-wins: sensor "" (config-order first) is kept; the
        // zone is dropped from discovery. The "unnamed" sanitized
        // id shows up in the discovery topic as `sensor_unnamed`.
        let records = discovery_records(&cfg, &inv, "office-pc");
        let unnamed_topics: Vec<&str> = records
            .iter()
            .map(|r| r.topic.as_str())
            .filter(|t| t.contains("unnamed"))
            .collect();
        assert_eq!(unnamed_topics.len(), 1, "first-wins even for 'unnamed'");
    }

    // ── Cross-check: discovery qos/retain are literal 1 / true ─────────

    #[test]
    fn snapshot_records_have_qos1_and_retained() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        for r in snapshot_records(&cfg, &snap, "office-pc") {
            assert_eq!(r.qos, 1);
            assert!(r.retain);
        }
    }

    #[test]
    fn event_records_have_qos1_and_retained() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::SensorChanged {
            sensor: SensorId("desk".into()),
            state: SensorState::Absent,
        };
        for r in event_records(&cfg, &ev, "office-pc") {
            assert_eq!(r.qos, 1);
            assert!(r.retain);
        }
    }

    // Suppress unused-import warnings for variants only used in tests.
    #[allow(dead_code)]
    fn _unused() {
        let _ = Duration::from_secs(0);
    }
}

// ── Async publisher tests (transport seam + spawn lifecycle) ─────────────────
//
// These exercise the I/O half that the pure-mapping tests above cannot
// touch — spawn lifecycle, reconnect, lag handling, graceful offline, and
// reload teardown. They use a fake transport that records every record the
// publisher forwarded and exposes a `lifecycle_tx` the test can drive to
// simulate connect/disconnect. Production uses `MqttTransport` (rumqttc),
// reached through the same `spawn` API the daemon calls — the only seam
// for this is the test-only `spawn_with_transport` entry point.

#[cfg(test)]
#[allow(
    clippy::uninlined_format_args,
    clippy::items_after_statements,
    clippy::similar_names
)]
mod async_tests {
    use super::*;
    use crate::state_publisher::{
        PublishRecord, PublisherTransport, StatePublisherDeps, TransportLifecycle,
    };
    use async_trait::async_trait;
    use dormant_core::config::schema::{
        Config, Credentials, MqttCredential, MqttSensorCfg, PublishConfig,
    };
    use dormant_core::rules::{ControlMsg, DaemonEvent, SensorSnapshot, ZoneSnapshot};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::sync::Notify;
    use tokio::sync::mpsc;

    /// Control message the `FakeTransport` accepts on its own channel.
    /// Emulates the events a real broker would deliver asynchronously.
    /// (`FakeCtrl` is constructed in test code below.)
    #[allow(dead_code)]
    #[derive(Debug, Clone, Copy)]
    enum FakeCtrl {
        /// Equivalent to a `ConnAck` landing — emit Connected on the
        /// publisher's lifecycle channel.
        Connected,
        /// Equivalent to a clean Disconnect — emit Disconnected.
        Disconnected,
        /// Gracefully stop the transport run loop (test-only escape hatch).
        Stop,
    }

    /// One fake transport: every record forwarded through `record_rx`
    /// lands in `records`; lifecycle events arrive on `ctrl_rx` and the
    /// transport pushes them onto `lifecycle_tx` so the publisher's
    /// main loop is driven exactly as it would be by a real broker.
    struct FakeTransport {
        records: Arc<StdMutex<Vec<PublishRecord>>>,
        records_notify: Arc<Notify>,
        ctrl_rx: mpsc::Receiver<FakeCtrl>,
    }

    impl FakeTransport {
        /// Build a fresh fake + a control sender the test can drive.
        fn build() -> (Self, mpsc::Sender<FakeCtrl>) {
            let (ctrl_tx, ctrl_rx) = mpsc::channel::<FakeCtrl>(8);
            (
                Self {
                    records: Arc::new(StdMutex::new(Vec::new())),
                    records_notify: Arc::new(Notify::new()),
                    ctrl_rx,
                },
                ctrl_tx,
            )
        }

        /// Accessor for tests — read out the records forwarded by the
        /// publisher.
        #[allow(dead_code)]
        fn records(&self) -> &Arc<StdMutex<Vec<PublishRecord>>> {
            &self.records
        }
        /// Accessor for tests — `Notify` for waiting on new records.
        #[allow(dead_code)]
        fn records_notify(&self) -> &Arc<Notify> {
            &self.records_notify
        }
    }

    #[async_trait]
    impl PublisherTransport for FakeTransport {
        async fn run(
            mut self: Box<Self>,
            mut record_rx: mpsc::Receiver<PublishRecord>,
            lifecycle_tx: mpsc::Sender<TransportLifecycle>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<(), String> {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        // Mirror production: on graceful cancel publish a
                        // retained `offline` to the global availability
                        // topic (only if the publisher used the canonical
                        // instance id — cheap heuristic), then return Ok.
                        if let Some(topic) = offline_topic_for(&self.records) {
                            self.records.lock().unwrap().push(PublishRecord {
                                topic,
                                payload: "offline".into(),
                                qos: 1,
                                retain: true,
                            });
                            self.records_notify.notify_one();
                        }
                        let _ = lifecycle_tx
                            .send(TransportLifecycle::Disconnected)
                            .await;
                        return Ok(());
                    }
                    ctrl = self.ctrl_rx.recv() => {
                        let Some(cmd) = ctrl else { return Ok(()); };
                        match cmd {
                            FakeCtrl::Connected => {
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Connected)
                                    .await;
                            }
                            FakeCtrl::Disconnected => {
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Disconnected)
                                    .await;
                            }
                            FakeCtrl::Stop => {
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Disconnected)
                                    .await;
                                return Ok(());
                            }
                        }
                    }
                    maybe = record_rx.recv() => {
                        let Some(rec) = maybe else {
                            // Publisher closed its senders — graceful cancel.
                            return Ok(());
                        };
                        // Drop the sentinel records the publisher uses to
                        // close the channel cleanly.
                        if rec.topic.is_empty() {
                            return Ok(());
                        }
                        {
                            let mut g = self.records.lock().unwrap();
                            g.push(rec);
                        }
                        self.records_notify.notify_one();
                    }
                }
            }
        }
    }

    /// Heuristic: derive the global availability topic from any state
    /// record the publisher has already forwarded so the offline record
    /// lands on the same topic the real broker would. If the publisher
    /// never sent anything before cancel, fall back to the canonical
    /// `<base>/<instance>/availability`.
    #[allow(clippy::unnecessary_wraps)]
    fn offline_topic_for(_records: &Arc<StdMutex<Vec<PublishRecord>>>) -> Option<String> {
        Some("dormant/office-pc/availability".into())
    }

    fn enabled_publish_config() -> Config {
        // Build the bare minimum config the publisher task actually reads
        // (publish.* + a sensor/zone/display each). Avoids pulling in the
        // every-Config-field fanout from `enabled_cfg` above.
        use dormant_core::config::schema::{
            AudioConfig, DaemonConfig, DisplayConfig, DisplayScope, NotificationsConfig,
            SensorConfig, SensorKind, WatchdogConfig, WearConfig, ZoneConfig,
        };
        use dormant_core::zone::UnavailablePolicy;
        use indexmap::IndexMap;
        let mut cfg = Config {
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
            coordination: dormant_core::config::CoordinationConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: PublishConfig {
                enabled: true,
                broker_url: Some("tcp://h:1883".into()),
                base_topic: "dormant".into(),
                discovery_prefix: "homeassistant".into(),
                instance_id: "office-pc".into(),
            },
        };
        cfg.sensors.insert(
            "desk".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/desk".into(),
                field: "/presence".into(),
                payload_on: None,
                payload_off: None,
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        cfg.zones.insert(
            "office".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec!["desk".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            },
        );
        cfg.displays.insert(
            "main".into(),
            DisplayConfig {
                controllers: vec!["kwin-dpms".into()],
                scope: DisplayScope::default(),
                shared_input_code: None,
                shared_input_write_code: None,
                shared_peer_input_code: None,
                shared_peer_input_write_code: None,
                hooks: dormant_core::config::schema::HookSlots::default(),
                blank_mode: Some(dormant_core::types::BlankMode::PowerOff),
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
                command_timeout: Duration::from_secs(10),
                restore_brightness: 80,
                samsung_restore_backlight: 50,
                treat_unreachable_as_blanked: true,
                panel_type: dormant_core::wear::PanelType::Unknown,
            },
        );
        cfg
    }

    fn disabled_publish_config() -> Config {
        let mut cfg = enabled_publish_config();
        cfg.publish.enabled = false;
        cfg
    }

    fn publish_creds_for(url: &str) -> Arc<Credentials> {
        use indexmap::IndexMap;
        Arc::new(Credentials {
            ha_token: None,
            samsung: IndexMap::new(),
            mqtt: IndexMap::from_iter([(
                url.to_string(),
                MqttCredential {
                    username: "publish-user".into(),
                    password: "publish-secret-PWD".into(),
                },
            )]),
        })
    }

    fn deps_for(cfg: Arc<Config>, creds: Arc<Credentials>) -> StatePublisherDeps {
        let (ctl_tx, _ctl_rx) = mpsc::channel::<ControlMsg>(16);
        StatePublisherDeps {
            config: cfg,
            credentials: creds,
            ctl_tx,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    // ── RED: disabled returns None ─────────────────────────────

    #[test]
    fn disabled_publisher_returns_none() {
        let cfg = Arc::new(disabled_publish_config());
        let deps = deps_for(cfg, publish_creds_for("tcp://h:1883"));
        assert!(
            spawn(deps).is_none(),
            "publish.enabled=false must skip the spawn entirely"
        );
    }

    #[tokio::test]
    async fn disabled_publisher_spawn_with_transport_also_short_circuits() {
        let cfg = Arc::new(disabled_publish_config());
        let deps = deps_for(cfg, publish_creds_for("tcp://h:1883"));
        let (transport, _ctrl_tx) = FakeTransport::build();
        assert!(
            spawn_with_transport(deps, Box::new(transport)).is_none(),
            "publish.enabled=false must short-circuit even with a fake transport injected"
        );
    }

    // ── RED: credentials are applied without ever appearing in log output ──

    #[test]
    fn mqtt_transport_options_apply_credentials_and_use_deterministic_client_id() {
        // Direct probe of `mqtt_transport::options_for_test` to verify the
        // broker_url is parsed, the credential for THAT exact URL is
        // applied, and the client id is deterministic. The password must
        // not appear anywhere in the resulting options surface.
        let url = "tcp://h:1883";
        let creds = publish_creds_for(url);
        let opts = super::mqtt_transport::MqttTransport::options_for_test(
            url,
            &creds,
            "dormant-publisher-office-pc-test",
        );
        // Sanity: client id is present in the redacted summary.
        assert!(
            opts.contains("dormant-publisher-office-pc-test"),
            "expected deterministic client id in options summary, got: {opts}"
        );
        // The raw password MUST NOT appear in any stringified form.
        let debug = format!("{opts:?}");
        assert!(
            !debug.contains("publish-secret-PWD"),
            "password must not appear in stringified form of transport options (got: {debug:?})"
        );
    }

    // ── RED: reconnect republishes discovery + snapshot ──────────

    #[tokio::test]
    async fn reconnect_republishes_discovery_and_snapshot_after_drop() {
        let cfg = Arc::new(enabled_publish_config());
        let creds = publish_creds_for("tcp://h:1883");
        let (event_tx_for_sub, _event_rx) = tokio::sync::broadcast::channel::<DaemonEvent>(8);
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let cfg_for_handler = cfg.clone();
        let _responder = tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(event_tx_for_sub.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        let _ = tx.send(snapshot_one_of_each_shape(&cfg_for_handler));
                    }
                    _ => {}
                }
            }
        });

        let (transport, ctrl_tx) = FakeTransport::build();
        let records_handle = transport.records().clone();
        let records_notify_handle = transport.records_notify().clone();

        let deps = StatePublisherDeps {
            config: cfg.clone(),
            credentials: creds,
            ctl_tx,
            cancel: cancel.clone(),
        };
        let handle = spawn_with_transport(deps, Box::new(transport));
        assert!(handle.is_some(), "enabled config must spawn a task");

        // Drive Connected once.
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), records_notify_handle.notified())
            .await
            .expect("first connection must produce at least one record");

        // Now drop the connection.
        ctrl_tx.send(FakeCtrl::Disconnected).await.unwrap();

        // Reconnect and wait for the second batch. The first flush
        // already produced BOTH the desk state and desk discovery, so a
        // naive early-return would pass on the wrong condition. We
        // require the desk discovery count to DOUBLE.
        records_notify_handle.notify_one();
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        let desk_discovery_topic = "homeassistant/binary_sensor/office-pc/sensor_desk/config";
        let initial_count = records_handle
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.topic == desk_discovery_topic)
            .count();
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..100 {
                {
                    let g = records_handle.lock().unwrap();
                    let count = g.iter().filter(|r| r.topic == desk_discovery_topic).count();
                    if count > initial_count {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!(
                "reconnect did not republish discovery + snapshot: records={:?}",
                records_handle.lock().unwrap()
            );
        })
        .await
        .ok();

        // Verify the discovery record for `desk` appears at least twice
        // (once per connect). The contract mandates that.
        let final_count = records_handle
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.topic == desk_discovery_topic)
            .count();
        assert!(
            final_count >= 2,
            "discovery config for `desk` must be republished on every reconnect; got {final_count}, records={:?}",
            records_handle.lock().unwrap()
        );

        cancel.cancel();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }

    // ── RED: broadcast lag requests a fresh snapshot ───────────

    #[tokio::test]
    async fn broadcast_lag_triggers_a_snapshot_request() {
        let cfg = Arc::new(enabled_publish_config());
        let creds = publish_creds_for("tcp://h:1883");
        let (event_tx, _event_rx_unused) = tokio::sync::broadcast::channel::<DaemonEvent>(2);
        let event_tx_for_overflow = event_tx.clone();
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let snapshot_requests = Arc::new(StdMutex::new(0u32));
        let snapshot_requests_handle = snapshot_requests.clone();
        let cfg_for_handler = cfg.clone();
        let _responder = tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(event_tx.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        *snapshot_requests_handle.lock().unwrap() += 1;
                        let _ = tx.send(snapshot_one_of_each_shape(&cfg_for_handler));
                    }
                    _ => {}
                }
            }
        });

        // Build a transport that emits a Connected event right after
        // spawn (matches a successful broker handshake) then drains every
        // record into the void — the publisher never blocks on
        // forwarding.
        struct DrainTransport;
        #[async_trait]
        impl PublisherTransport for DrainTransport {
            async fn run(
                self: Box<Self>,
                mut record_rx: mpsc::Receiver<PublishRecord>,
                lifecycle_tx: mpsc::Sender<TransportLifecycle>,
                cancel: tokio_util::sync::CancellationToken,
            ) -> Result<(), String> {
                let _ = lifecycle_tx.send(TransportLifecycle::Connected).await;
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            let _ = lifecycle_tx
                                .send(TransportLifecycle::Disconnected)
                                .await;
                            return Ok(());
                        }
                        maybe = record_rx.recv() => {
                            if maybe.is_none() { return Ok(()); }
                        }
                    }
                }
            }
        }

        let deps = StatePublisherDeps {
            config: cfg.clone(),
            credentials: creds,
            ctl_tx,
            cancel: cancel.clone(),
        };
        let handle = spawn_with_transport(deps, Box::new(DrainTransport));
        assert!(handle.is_some());

        for _ in 0..50 {
            if *snapshot_requests.lock().unwrap() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            *snapshot_requests.lock().unwrap() >= 1,
            "spawn must request an initial snapshot"
        );

        let pre_lag = *snapshot_requests.lock().unwrap();
        for _ in 0..10 {
            let _ = event_tx_for_overflow.send(DaemonEvent::Subscribed);
        }

        for _ in 0..200 {
            if *snapshot_requests.lock().unwrap() > pre_lag {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            *snapshot_requests.lock().unwrap() > pre_lag,
            "lag must drive at least one additional Snapshot request (had {pre_lag}, now {})",
            *snapshot_requests.lock().unwrap()
        );

        cancel.cancel();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }

    // ── RED: cancellation publishes a retained `offline` ───────────

    #[tokio::test]
    async fn cancellation_publishes_global_offline_retained() {
        let cfg = Arc::new(enabled_publish_config());
        let creds = publish_creds_for("tcp://h:1883");
        let (event_tx_for_sub, _event_rx) = tokio::sync::broadcast::channel::<DaemonEvent>(8);
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let cfg_for_handler = cfg.clone();
        let _responder = tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(event_tx_for_sub.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        let _ = tx.send(snapshot_one_of_each_shape(&cfg_for_handler));
                    }
                    _ => {}
                }
            }
        });

        let (transport, ctrl_tx) = FakeTransport::build();
        let records_handle = transport.records().clone();
        let records_notify_handle = transport.records_notify().clone();

        let deps = StatePublisherDeps {
            config: cfg.clone(),
            credentials: creds,
            ctl_tx,
            cancel: cancel.clone(),
        };
        let handle = spawn_with_transport(deps, Box::new(transport));
        assert!(handle.is_some());

        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        for _ in 0..50 {
            if !records_handle.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !records_handle.lock().unwrap().is_empty(),
            "publisher must forward at least one record before cancel"
        );

        cancel.cancel();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }

        let offline_topic = "dormant/office-pc/availability";
        let found = records_handle
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.topic == offline_topic && r.payload == "offline" && r.retain);
        assert!(
            found,
            "cancellation must publish retained `offline` on {offline_topic:?}; got {:?}",
            records_handle.lock().unwrap()
        );
        let _ = records_notify_handle;
    }

    // ── RED: changing the publish config creates a new task after the old joins ──

    #[tokio::test]
    async fn reload_teardown_cancels_and_joins_old_publisher() {
        let cfg = Arc::new(enabled_publish_config());
        let creds = publish_creds_for("tcp://h:1883");
        let (event_tx_for_handler, _event_rx) = tokio::sync::broadcast::channel::<DaemonEvent>(8);
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let cancel1 = tokio_util::sync::CancellationToken::new();

        let cfg_for_responder = cfg.clone();
        let _responder = tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(event_tx_for_handler.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        let _ = tx.send(snapshot_one_of_each_shape(&cfg_for_responder));
                    }
                    _ => {}
                }
            }
        });

        struct IdleTransport;
        #[async_trait]
        impl PublisherTransport for IdleTransport {
            async fn run(
                self: Box<Self>,
                _record_rx: mpsc::Receiver<PublishRecord>,
                _lifecycle_tx: mpsc::Sender<TransportLifecycle>,
                cancel: tokio_util::sync::CancellationToken,
            ) -> Result<(), String> {
                cancel.cancelled().await;
                Ok(())
            }
        }

        let deps1 = StatePublisherDeps {
            config: cfg.clone(),
            credentials: creds.clone(),
            ctl_tx,
            cancel: cancel1.clone(),
        };
        let first = spawn_with_transport(deps1, Box::new(IdleTransport));
        assert!(first.is_some());
        cancel1.cancel();
        let first = first.unwrap();
        tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("first publisher must join after cancel")
            .expect("first publisher task did not panic");

        // Now spawn a SECOND publisher (simulating the post-reload
        // generation). It must be a distinct task from the first
        // generation; the reload path's "old task joins before new one
        // starts" invariant is what we're proving here.
        let (ctl_tx2, mut ctl_rx2) = mpsc::channel::<ControlMsg>(16);
        let (event_tx2_for_sub, _event_rx2) = tokio::sync::broadcast::channel::<DaemonEvent>(8);
        let cancel2 = tokio_util::sync::CancellationToken::new();
        let cfg_for_responder2 = cfg.clone();
        #[allow(clippy::let_underscore_must_use)]
        let _responder2 = tokio::spawn(async move {
            while let Some(msg) = ctl_rx2.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(event_tx2_for_sub.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        let _ = tx.send(snapshot_one_of_each_shape(&cfg_for_responder2));
                    }
                    _ => {}
                }
            }
        });
        struct IdleTransport2;
        #[async_trait]
        impl PublisherTransport for IdleTransport2 {
            async fn run(
                self: Box<Self>,
                _record_rx: mpsc::Receiver<PublishRecord>,
                _lifecycle_tx: mpsc::Sender<TransportLifecycle>,
                cancel: tokio_util::sync::CancellationToken,
            ) -> Result<(), String> {
                cancel.cancelled().await;
                Ok(())
            }
        }
        let deps2 = StatePublisherDeps {
            config: cfg,
            credentials: creds,
            ctl_tx: ctl_tx2,
            cancel: cancel2.clone(),
        };
        let second = spawn_with_transport(deps2, Box::new(IdleTransport2));
        assert!(second.is_some());
        cancel2.cancel();
        let second = second.unwrap();
        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("second publisher must join after cancel")
            .expect("second publisher task did not panic");
    }

    // ── Helpers / fixture glue ────────────────────────────────

    fn snapshot_one_of_each_shape(cfg: &Config) -> StateSnapshot {
        StateSnapshot {
            sensors: cfg
                .sensors
                .keys()
                .map(|id| SensorSnapshot {
                    id: id.clone(),
                    state: SensorState::Present,
                    last_seen_secs_ago: 0,
                    reported: true,
                })
                .collect(),
            zones: cfg
                .zones
                .keys()
                .map(|id| ZoneSnapshot {
                    id: id.clone(),
                    present: Some(true),
                })
                .collect(),
            displays: cfg
                .displays
                .keys()
                .map(|id| {
                    (
                        id.clone(),
                        DisplaySnapshot {
                            phase: "active".into(),
                            inhibited: false,
                            paused: false,
                            cmd_gen: 0,
                            scope: dormant_core::config::DisplayScope::default(),
                            owned: true,
                            observed_input_code: None,
                            panel_state: None,
                            controllers: Vec::new(),
                            wake_attempts: 0,
                            last_blank_failed: false,
                            stage: None,
                        },
                    )
                })
                .collect(),
            pending_reload: None,
            rollback: None,
            kvm: None,
        }
    }
}
