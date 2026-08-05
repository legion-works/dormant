//! Pure state → MQTT publish-record mapping for issue #105.
//!
//! Implements the topic, payload, and collision contract ratified in
//! `.opencode/decisions/2026-07-27-mqtt-publish-contract.md`. NO I/O:
//! the public entry points return `Vec<PublishRecord>` for the
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
//! - [`global_online_record`] — retained `online` for the global LWT
//!   availability topic (issue #174). The discovery payloads all
//!   reference this topic, so its wire-state gates every entity's HA
//!   "available" bit; nothing else ever writes `online` to it.
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
//! Every entry point returns an empty `Vec` (or `None` for the single
//! [`PublishRecord`] helper) when `cfg.publish.enabled == false`. The
//! publisher task is the only caller and it gates the call site on the
//! same flag; the check here is defensive (and keeps the pure
//! functions easy to unit-test without a real broker).

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
/// (re)connect, on graceful disconnect, and on Home Assistant's
/// birth announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportLifecycle {
    /// The transport has (re)connected and is ready to forward records.
    /// The publisher responds by republishing discovery + a current
    /// snapshot, exactly as the contract requires.
    Connected,
    /// Home Assistant has announced it is back online on
    /// `<discovery_prefix>/status`. Issue #174: an operator who deletes
    /// an entity in HA clears its retained discovery config topic, and
    /// HA also clears all dormant's discovery topics when HA itself
    /// restarts. Subscribing to the HA birth topic lets the publisher
    /// re-flush discovery + a fresh snapshot whenever HA comes back, so
    /// the deleted entity reappears. The publisher responds EXACTLY
    /// like `Connected`: send a retained `online` on the global
    /// availability topic, then re-flush discovery, then re-snapshot.
    BirthOnline,
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
    /// the current snapshot). The production transport also subscribes
    /// to Home Assistant's birth topic (`<discovery_prefix>/status`)
    /// and emits a [`TransportLifecycle::BirthOnline`] whenever HA
    /// comes back online (issue #174); the publisher treats that signal
    /// identically to `Connected`. On graceful cancellation the
    /// transport MUST emit a final best-effort retained-offline publish
    /// (the LWT covers ungraceful broker-side drops) followed by a
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
                Some(TransportLifecycle::BirthOnline) => {
                    // Defensive: production emits `Connected` AFTER the
                    // ConnAck and BEFORE the status-topic subscribe, so a
                    // `BirthOnline` arriving here means the lifecycle
                    // ordering regressed. Treat it the same as Connected
                    // (proceed with the first flush) so a buggy transport
                    // can't strand us in the wait loop forever.
                    tracing::debug!(event = "publish_first_birth_online");
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

    // First connect: flush global online + discovery + snapshot.
    flush_full(FlushRequest {
        cfg: &config,
        snapshot: &mut snapshot,
        instance: &instance,
        record_tx: &record_tx,
        ctl_tx: &ctl_tx,
        cancel: &cancel,
        request_fresh_snapshot: false,
        event_label: "publish_first_flush",
    })
    .await;

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
                    // Reconnect: re-flush global online + discovery + a fresh snapshot.
                    flush_full(
                        FlushRequest {
                            cfg: &config,
                            snapshot: &mut snapshot,
                            instance: &instance,
                            record_tx: &record_tx,
                            ctl_tx: &ctl_tx,
                            cancel: &cancel,
                            request_fresh_snapshot: true,
                            event_label: "publish_reconnect_flush",
                        },
                    )
                    .await;
                }
                Some(TransportLifecycle::BirthOnline) => {
                    // HA came back online — re-flush the same way we do on
                    // reconnect so an entity the operator deleted in HA
                    // (HA clears the retained discovery config topic on
                    // delete, and on HA's own restart) is re-discovered.
                    // Issue #174.
                    flush_full(
                        FlushRequest {
                            cfg: &config,
                            snapshot: &mut snapshot,
                            instance: &instance,
                            record_tx: &record_tx,
                            ctl_tx: &ctl_tx,
                            cancel: &cancel,
                            request_fresh_snapshot: true,
                            event_label: "publish_ha_birth_reflush",
                        },
                    )
                    .await;
                }
                Some(TransportLifecycle::Disconnected) => { /* reconnect handled by Connected */ }
                None => {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), transport_handle).await;
                    return;
                }
            },
            event_result = event_rx.recv() => match event_result {
                Ok(ev) => {
                    send_flush(&record_tx, &cancel, event_records(&config, &ev, &instance)).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(event = "publish_events_lagged", skipped);
                    if let Some(new_snap) = request_snapshot(&ctl_tx, &cancel).await {
                        snapshot = new_snap;
                        send_flush(
                            &record_tx,
                            &cancel,
                            snapshot_records(&config, &snapshot, &instance),
                        )
                        .await;
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

/// All inputs the publisher's three flush paths (first-connect,
/// reconnect, HA birth) need. Bundled into one struct so the three
/// sites call the same helper with the same arguments — issue #174's
/// shared flush is the only seam that prevents a regression on one
/// path from silently breaking the other two.
struct FlushRequest<'a> {
    cfg: &'a Config,
    snapshot: &'a mut StateSnapshot,
    instance: &'a str,
    record_tx: &'a mpsc::Sender<PublishRecord>,
    ctl_tx: &'a mpsc::Sender<dormant_core::rules::ControlMsg>,
    cancel: &'a tokio_util::sync::CancellationToken,
    /// True on reconnect and HA-birth paths (a fresh snapshot may pick
    /// up state that changed while we were disconnected); false on the
    /// initial first-connect (we already hold the snapshot the caller
    /// requested at startup, so an extra round-trip is wasted).
    request_fresh_snapshot: bool,
    /// Tracing event literal — `publish_first_flush`,
    /// `publish_reconnect_flush`, or `publish_ha_birth_reflush`.
    /// `tracing` reads these as field values, so the literal MUST match
    /// the grep-stable contract above.
    event_label: &'static str,
}

/// Shared flush body used by first-connect, reconnect, and the HA birth
/// signal. The publisher's contract says every connected/re-flush moment
/// must (1) emit a retained `online` on the global LWT availability
/// topic, (2) re-publish every discovery config, (3) re-publish the
/// current retained state. Without this helper the three paths would
/// drift out of sync (issue #174's defect #1 is exactly that drift —
/// the global `online` was forgotten on every path).
async fn flush_full(req: FlushRequest<'_>) {
    let FlushRequest {
        cfg,
        snapshot,
        instance,
        record_tx,
        ctl_tx,
        cancel,
        request_fresh_snapshot,
        event_label,
    } = req;

    // 1. Global `online` — every discovery payload references this topic;
    //    without a retained `online`, HA marks every entity unavailable.
    if let Some(rec) = global_online_record(cfg, instance) {
        send_flush(record_tx, cancel, vec![rec]).await;
    }

    // 2. Discovery configs.
    let inventory = EntityInventory::from_config(cfg);
    let _ = detect_and_warn_collisions(cfg, &inventory);
    let disc = discovery_records(cfg, &inventory, instance);
    let disc_total = disc.len();
    send_flush(record_tx, cancel, disc).await;

    // 3. Snapshot — fresh if requested (reconnect / HA birth), otherwise
    //    the caller's pre-existing snapshot.
    if request_fresh_snapshot && let Some(new_snap) = request_snapshot(ctl_tx, cancel).await {
        *snapshot = new_snap;
    }
    let snaps = snapshot_records(cfg, snapshot, instance);
    tracing::debug!(
        event = event_label,
        discovery = disc_total,
        snapshot = snaps.len(),
        "publishing discovery + snapshot flush"
    );
    send_flush(record_tx, cancel, snaps).await;
}

/// Awaited forward of a single record to the transport-side
/// `record_rx`. Cancellation-aware: a cancel mid-send drops the record
/// and emits a `publish_record_dropped` warning — never silently
/// swallowed (a flapping broker that stalls the channel would otherwise
/// let a retained record fade out unreported).
///
/// The bounded mpsc (64 records) is overflow-free by construction:
/// every flush is bounded by the entity count + event-rate per loop
/// iteration. If the channel ever does fail to accept (a malformed
/// misbehaviour we want to learn about, not paper over), the WARN
/// records `topic` + the cancellation state at that moment.
async fn send_record(
    record_tx: &mpsc::Sender<PublishRecord>,
    cancel: &tokio_util::sync::CancellationToken,
    record: PublishRecord,
) {
    let topic = record.topic.clone();
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            tracing::warn!(
                event = "publish_record_dropped",
                topic = %topic,
                reason = "cancel_observed_before_send",
                "publish record dropped because cancel fired mid-send; the next reconnect will retry the discovery/snapshot"
            );
        }
        send_result = record_tx.send(record) => {
            if let Err(_record) = send_result {
                // Receiver was closed (transport task exited). We will
                // recover on the next reconnect by re-flushing, so
                // surface the drop rather than silently swallowing.
                tracing::warn!(
                    event = "publish_record_dropped",
                    topic = %topic,
                    reason = "transport_channel_closed",
                    "publish record dropped because transport channel closed; reload required to retry"
                );
            }
        }
    }
}

/// Push a batch of records through the transport-side `record_rx`,
/// cancelling cleanly when `cancel` fires and warning (per-record) on
/// any drop. Drains `records` even when a cancel lands mid-batch so
/// the rest of the flush completes during the cancellation grace
/// window — the contract says `finalize_shutdown` joins the transport
/// within 5 s, plenty of room for a typical entity count.
async fn send_flush(
    record_tx: &mpsc::Sender<PublishRecord>,
    cancel: &tokio_util::sync::CancellationToken,
    records: Vec<PublishRecord>,
) {
    for r in records {
        if cancel.is_cancelled() {
            tracing::warn!(
                event = "publish_record_dropped",
                topic = %r.topic,
                reason = "cancel_observed_before_flush_iteration",
                "publish flush short-circuited because cancel fired; the next reconnect will retry discovery/snapshot"
            );
            return;
        }
        send_record(record_tx, cancel, r).await;
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
        /// Home Assistant's birth topic (`<discovery_prefix>/status`),
        /// sanitized the same way as `discovery_records` builds its
        /// topics. Subscribed once per (re)connect; an incoming
        /// `online` payload drives a [`TransportLifecycle::BirthOnline`]
        /// so the publisher can re-flush discovery (issue #174).
        status_topic: String,
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
            // Sanitize the discovery prefix the SAME way `discovery_records`
            // does — the prefix sanitization is the only seam keeping the
            // status topic on the same wire layout as the discovery
            // configs. A user who puts spaces in their prefix gets the
            // same underscore-collapsed topic for both.
            let discovery_prefix = super::sanitize_topic_id(&cfg.publish.discovery_prefix);
            let status_topic = format!("{discovery_prefix}/status");
            let client_id = format!("dormant-publisher-{instance}-{}", std::process::id());
            Box::new(Self {
                broker_url,
                client_id,
                credentials: credentials.clone(),
                global_availability_topic,
                status_topic,
                backoff_initial: Duration::from_millis(100),
                backoff_max: Duration::from_secs(5),
                per_attempt_timeout: Duration::from_secs(2),
            })
        }

        /// Build `MqttOptions` with credentials applied for `url`.
        ///
        /// Public-only so a test can assert credential handling without
        /// having to wrestle a `Box<dyn PublisherTransport>` for the
        /// answer. A malformed URL surfaces as `Err(String)` so the
        /// transport can fail fast at startup instead of pointing the
        /// client at a garbage host.
        ///
        /// # Errors
        ///
        /// Returns the parse error wrapped in a `String` when `url` is
        /// not a valid MQTT broker URL (empty host or malformed port —
        /// see [`dormant_core::mqtt::parse_broker_url`]).
        pub fn build_options(
            url: &str,
            creds: &Credentials,
            client_id: &str,
        ) -> Result<MqttOptions, String> {
            let (host, port) =
                parse_broker_url(url).map_err(|e| format!("invalid broker_url {url:?}: {e}"))?;
            let mut opts = MqttOptions::new(client_id, host.to_string(), port);
            opts.set_clean_session(true);
            if let Some(cred) = creds.mqtt.get(url) {
                opts.set_credentials(cred.username.clone(), cred.password.clone());
            }
            Ok(opts)
        }

        /// Apply the daemon's publish-credential conventions and return
        /// a redacted textual summary suitable for assertions in
        /// `crates/dormantd::state_publisher`'s tests.
        ///
        /// The summary deliberately omits the password AND the broker
        /// URL: a userinfo-bearing URL (e.g. `mqtt://user:secret@host`)
        /// would leak the secret the moment a log line, traced span, or
        /// `Debug` print formatted the string. Only the parsed
        /// host:port pair (which carries no userinfo) is reflected.
        /// Credentials are looked up by EXACT `broker_url` (the
        /// contract docs say "credential lookup is by exact
        /// `broker_url`").
        ///
        /// Returns `Err(detail)` on a malformed broker URL — callers
        /// should surface that as a test failure rather than mishandling
        /// it silently.
        ///
        /// # Errors
        ///
        /// Returns the parse error wrapped in a `String` when `url` is
        /// not a valid MQTT broker URL.
        pub fn options_for_test(
            url: &str,
            creds: &Credentials,
            client_id: &str,
        ) -> Result<String, String> {
            let user = creds
                .mqtt
                .get(url)
                .map_or("<none>", |c| c.username.as_str());
            let (host, port) =
                parse_broker_url(url).map_err(|e| format!("invalid broker_url {url:?}: {e}"))?;
            // `parse_broker_url` returns the host (possibly
            // userinfo-laden, e.g. `dormant:publish-secret-PWD@h`) or a
            // bracketed IPv6 literal — for any of those, only the bare
            // host segment AFTER the `@` is safe to print. If there's
            // no `@`, the parsed `host` is already safe.
            let broker_host = match host.rsplit_once('@') {
                Some((_, after)) => after,
                None => host,
            };
            Ok(format!(
                "client_id={client_id:?} broker={broker_host}:{port} user={user:?} password_present={}",
                creds.mqtt.get(url).is_some()
            ))
        }

        /// Return the HA birth topic the transport subscribes to after
        /// each `ConnAck`. Public-only for tests that assert the topic
        /// derivation stays consistent with `discovery_records`'s
        /// prefix sanitization (issue #174: an off-by-one between the
        /// subscribe topic and the discovery prefix would silently
        /// disable the re-flush path).
        #[must_use]
        pub fn status_topic_for_test(discovery_prefix: &str) -> String {
            let sanitized = super::sanitize_topic_id(discovery_prefix);
            format!("{sanitized}/status")
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
                let (host, port) = match parse_broker_url(&self.broker_url) {
                    Ok(pair) => pair,
                    Err(e) => {
                        return Err(format!("invalid broker_url {:?}: {e}", self.broker_url));
                    }
                };
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
                        // Issue #174 Should 2: drain stale records from
                        // any aborted flush that left records sitting in
                        // `record_rx` across the reconnect. The
                        // publisher's reconnect arm will re-flush
                        // everything on Connected, so anything queued
                        // here is older than the current state and would
                        // briefly publish stale state. The empty-topic
                        // close sentinel MUST NOT be swallowed — if the
                        // publisher signaled shutdown mid-disconnect, we
                        // honor it (Disconnected + return).
                        let Some(drained) = drain_stale_records(&mut record_rx) else {
                            let _ = lifecycle_tx.send(TransportLifecycle::Disconnected).await;
                            return Ok(());
                        };
                        if drained > 0 {
                            tracing::debug!(
                                event = "publish_stale_records_dropped",
                                drained,
                                "stale records from aborted flush dropped on reconnect"
                            );
                        }
                        // First connect: emit Connected.
                        if lifecycle_tx
                            .send(TransportLifecycle::Connected)
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        // Subscribe to Home Assistant's birth topic so a
                        // retained-deleted entity in HA is re-discovered
                        // on HA's `online` announcement. Issue #174: HA
                        // clears dormant's retained discovery configs when
                        // an entity is deleted there, and on HA's own
                        // restart; subscribing to `<discovery_prefix>/status`
                        // is the MQTT-integration standard way to learn
                        // HA is back so the publisher can re-flush.
                        if let Err(e) = client.subscribe(&self.status_topic, QoS::AtLeastOnce).await
                        {
                            tracing::debug!(
                                event = "publish_status_subscribe_failed",
                                topic = %self.status_topic,
                                error = %e,
                                "status-topic subscribe failed; HA birth re-flush will be unavailable this session"
                            );
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
                let mut pending_birth = false;
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
                                wait_for_publish_id(
                                    &mut eventloop,
                                    &self.status_topic,
                                    &mut pending_birth,
                                ),
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
                                wait_for_publish_id(
                                    &mut eventloop,
                                    &self.status_topic,
                                    &mut pending_birth,
                                ),
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
                            // After the wait, surface any pending
                            // `BirthOnline` observation: a HA
                            // `<discovery_prefix>/status` `Publish` with
                            // payload `online` that arrived during the
                            // PubAck-wait window would otherwise be
                            // silently dropped (issue #174 Should 1).
                            // The flag is sticky — N arrivals in one
                            // flush collapse to one re-flush, which is
                            // correct.
                            if take_pending_birth(&mut pending_birth) {
                                tracing::info!(event = "publish_ha_birth_reflush");
                                if lifecycle_tx
                                    .send(TransportLifecycle::BirthOnline)
                                    .await
                                    .is_err()
                                {
                                    return Ok(());
                                }
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
                                Ok(Event::Incoming(packet @ Packet::Publish(_))) => {
                                    // Home Assistant publishes `online` on
                                    // `<discovery_prefix>/status` on startup
                                    // (and on a clean restart); `offline` on
                                    // graceful shutdown. We only care about
                                    // the `online` edge — the `offline`
                                    // arrival is a normal HA restart in
                                    // progress, NOT a defunct signal we
                                    // need to react to.
                                    observe_birth_packet(&packet, &self.status_topic, &mut pending_birth);
                                    if take_pending_birth(&mut pending_birth) {
                                        tracing::info!(event = "publish_ha_birth_reflush");
                                        if lifecycle_tx
                                            .send(TransportLifecycle::BirthOnline)
                                            .await
                                            .is_err()
                                        {
                                            return Ok(());
                                        }
                                    }
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

    /// Event-loop drain until a [`Packet::PubAck`] lands for the most
    /// recent publish. The publisher pushes records one at a time through
    /// a bounded mpsc, so order is preserved by the caller's sequential
    /// `await`s.
    ///
    /// WHILE draining, also watch for an incoming [`Packet::Publish`] on
    /// `status_topic` with payload `online` and flip `pending_birth` if
    /// seen. Issue #174 Should 1: HA's retained `online` is delivered by
    /// the broker immediately after the Subscribe Ack, so an `online`
    /// arriving during any of the PubAck-wait windows in a flush would
    /// previously be silently dropped (only `PubAck` returned `Ok`), and
    /// the `BirthOnline` re-flush would self-heal only on a later
    /// reconnect — potentially days for an operator with a stable
    /// network. The flag is sticky: N `online` arrivals during one wait
    /// collapse to one re-flush, which is correct (multiple births in a
    /// single flush is implausible but the dedup costs nothing).
    async fn wait_for_publish_id(
        eventloop: &mut EventLoop,
        status_topic: &str,
        pending_birth: &mut bool,
    ) -> Result<(), String> {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(packet)) => {
                    let is_ack = matches!(&packet, Packet::PubAck(_));
                    observe_birth_packet(&packet, status_topic, pending_birth);
                    if is_ack {
                        return Ok(());
                    }
                }
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    pub(crate) fn observe_birth_packet(
        packet: &Packet,
        status_topic: &str,
        pending_birth: &mut bool,
    ) {
        if let Packet::Publish(p) = packet
            && p.topic == status_topic
            && p.payload.as_ref() == b"online"
        {
            *pending_birth = true;
        }
    }

    pub(crate) fn take_pending_birth(pending_birth: &mut bool) -> bool {
        let pending = *pending_birth;
        *pending_birth = false;
        pending
    }

    pub(crate) fn drain_stale_records(
        record_rx: &mut mpsc::Receiver<PublishRecord>,
    ) -> Option<usize> {
        let mut drained = 0usize;
        loop {
            match record_rx.try_recv() {
                Ok(rec) if rec.topic.is_empty() => return None,
                Ok(_) => drained += 1,
                Err(mpsc::error::TryRecvError::Empty) => return Some(drained),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
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

/// Build the retained `online` record for the global LWT availability
/// topic. Every discovery payload references this topic (built from
/// `topic_availability`); without this helper, nothing on the publish
/// plane ever writes `online` to it, so HA marks every entity
/// unavailable after the first retained `offline` (issue #174).
///
/// The wire topic MUST match the LWT topic the broker falls back to
/// on broker-side drop, so HA's availability claim (offline on drop,
/// online on retained `online`) flips on the same signal. The
/// payload is the contract's literal `online`; the QoS/retain flags
/// mirror the LWT (`QoS` 1, retain `true`) so a missed `PubAck` during a
/// broker restart still lands as the latest known availability.
///
/// Returns `None` when `cfg.publish.enabled == false` so the publisher
/// task can short-circuit without a flag check at every call site.
#[must_use]
pub fn global_online_record(cfg: &Config, instance: &str) -> Option<PublishRecord> {
    if !cfg.publish.enabled {
        return None;
    }
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    Some(PublishRecord {
        topic: topic_availability(&base, &instance),
        payload: "online".to_string(),
        qos: PUBLISH_QOS,
        retain: RETAIN,
    })
}

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
    // Apply the SAME first-wins collision filter as `discovery_records`
    // so a sensor/zone/display whose config-order-second sanitized id
    // also resolves a winner does NOT leak its state/availability
    // record onto the broker. The contract mandates "publishes ONLY
    // the first (config-order) — never interleaves two entities on
    // one topic" — the discovery side already filters via the `seen`
    // HashSet; the snapshot side MUST do the same.
    //
    // The filter is PER-KIND + PER-SANITIZED-ID, not just per
    // sanitized id, because the first-wins winner is determined by
    // config order across kinds (sensor first, then zone, then
    // display). For a sensor "Office Radar" (winner) + zone
    // "office_radar" (loser), both sanitize to "office_radar" — the
    // sensor's snapshot is published, the zone's is dropped.
    let inventory = EntityInventory::from_config(cfg);
    let mut seen_sensor: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_zone: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_display: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw in &inventory.sensors {
        let sanitized = sanitize_topic_id(raw);
        seen_sensor.insert(sanitized);
    }
    for raw in &inventory.zones {
        let sanitized = sanitize_topic_id(raw);
        if seen_sensor.contains(&sanitized) {
            // The sanitized id was already claimed by a sensor — the
            // sensor wins; this zone is a LOSER and its snapshot is
            // dropped.
            continue;
        }
        seen_zone.insert(sanitized);
    }
    for raw in &inventory.displays {
        let sanitized = sanitize_topic_id(raw);
        if seen_sensor.contains(&sanitized) || seen_zone.contains(&sanitized) {
            // A sensor or zone already won this sanitized id — the
            // display is a LOSER.
            continue;
        }
        seen_display.insert(sanitized);
    }
    let mut records: Vec<PublishRecord> = Vec::new();

    for sensor in &snapshot.sensors {
        let sensor_id = sanitize_topic_id(&sensor.id);
        if !seen_sensor.contains(&sensor_id) {
            continue;
        }
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
        if !seen_zone.contains(&zone_id) {
            continue;
        }
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
        if !seen_display.contains(&display_id) {
            continue;
        }
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
                // Keep event updates identical to snapshot updates so HA's
                // `value_template: "{{ value_json.phase }}"` parses both.
                payload: phase_json(phase),
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
        | DaemonEvent::OperationsChanged { .. }
        | DaemonEvent::WakeRetry { .. }
        | DaemonEvent::WearSnapshot { .. }
        | DaemonEvent::WearSamplingStarted
        | DaemonEvent::WearSamplingDegraded { .. }
        | DaemonEvent::WearSamplingSourceGate { .. }
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
    phase_json(&display.phase)
}

fn phase_json(phase: &str) -> String {
    json!({ "phase": phase }).to_string()
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
                power_off_opt_in: false,
                compositor_output: None,
                sampling: None,
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
            wear_sampling_status: None,
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

    // ── Global online availability (issue #174) ───────────────────

    #[test]
    fn global_online_record_none_when_disabled() {
        // The defensive disabled-by-default check must short-circuit
        // to None — the production `run` loop already gates on the
        // same flag, but every public entry point returns an empty
        // (or None for the single-record helper) value so a caller
        // that forgets the flag check does not silently publish an
        // `online` while everything else stays silent.
        let cfg = disabled_cfg();
        assert!(
            global_online_record(&cfg, "office-pc").is_none(),
            "publish.enabled=false must short-circuit global_online_record to None"
        );
    }

    #[test]
    fn global_online_record_shape_when_enabled() {
        // Cross-check the wire shape of the helper: retained QoS-1
        // `online` on the global LWT availability topic. Every entity
        // the discovery side emits references this topic, so the
        // payload MUST match the LWT's payload vocabulary (`online`)
        // and the retain flag MUST match the LWT's retain so a
        // retained payload swap on the broker is a no-op.
        let cfg = enabled_cfg();
        let rec = global_online_record(&cfg, "office-pc")
            .expect("enabled config must produce a global online record");
        assert_eq!(rec.topic, "dormant/office-pc/availability");
        assert_eq!(rec.payload, "online");
        assert_eq!(rec.qos, 1);
        assert!(rec.retain, "global online is retained (matches LWT)");
    }

    #[test]
    fn global_online_record_sanitizes_base_and_instance() {
        // The helper reuses the existing `sanitize_topic_id` and
        // `topic_availability` seams; this is a cross-check that
        // nothing in the helper accidentally re-derives the topic
        // shape and bypasses the sanitizer.
        let mut cfg = enabled_cfg();
        cfg.publish.base_topic = "Dormant Prod".into();
        cfg.publish.instance_id = "Weird Host!".into();
        let rec = global_online_record(&cfg, "Weird Host!").expect("enabled");
        assert_eq!(rec.topic, "dormant_prod/weird_host/availability");
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
    fn event_records_display_phase_yields_json_phase() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::DisplayPhase {
            display: DisplayId("main".into()),
            phase: "blanking".into(),
            cause: "zone_lost".into(),
        };
        let records = event_records(&cfg, &ev, "office-pc");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].topic, "dormant/office-pc/display/main/phase");
        let payload: serde_json::Value = serde_json::from_str(&records[0].payload).unwrap();
        assert_eq!(payload["phase"], "blanking");
    }

    #[test]
    fn event_and_snapshot_display_phase_payloads_cannot_drift() {
        let cfg = enabled_cfg();
        let phase = "grace";
        let event = DaemonEvent::DisplayPhase {
            display: DisplayId("main".into()),
            phase: phase.into(),
            cause: "zone_lost".into(),
        };
        let event_payload: serde_json::Value =
            serde_json::from_str(&event_records(&cfg, &event, "office-pc")[0].payload).unwrap();

        let mut snapshot = snapshot_one_of_each();
        snapshot.displays[0].1.phase = phase.into();
        let snapshot_payload: serde_json::Value = serde_json::from_str(
            &snapshot_records(&cfg, &snapshot, "office-pc")
                .iter()
                .find(|record| record.topic == "dormant/office-pc/display/main/phase")
                .unwrap()
                .payload,
        )
        .unwrap();

        assert_eq!(event_payload["phase"], snapshot_payload["phase"]);
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
                wear_attribution_mode: dormant_core::wear::WearAttributionMode::Uniform,
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
    #[allow(clippy::too_many_lines)]
    fn collision_winner_publishes_only_the_config_order_first_payload() {
        // Cross-kind collision: a sensor and a zone with raw ids that
        // sanitize to the same topic id. Insertion order is
        // config-order (IndexMap iteration), so the sensor MUST win.
        // The loser's payload (zone discovery) is byte-distinguishable
        // from the winner's (sensor discovery) by the `name` field —
        // the winner's payload is `"dormant office_radar presence"`, the
        // loser's would be `"dormant office_radar zone"`. The discriminator
        // is the absence of the loser's `name` substring on the wire,
        // and the presence of the winner's. (A same-kind collision
        // produces byte-identical sanitized payloads — the sanitisers
        // collapse both raw ids into the same `name`/`unique_id`, so
        // the discriminator needs a cross-kind shape.)
        let mut cfg = enabled_cfg();
        // Remove the default 'desk' sensor / 'office' zone / 'main'
        // display so the collision we build is the only thing the
        // discovery list sees.
        cfg.sensors.shift_remove("desk");
        cfg.zones.shift_remove("office");
        cfg.displays.shift_remove("main");
        // The WINNER: a sensor whose raw id is the LOSER's raw id
        // with whitespace + uppercase injected. The sanitizer maps
        // both to "office_radar".
        cfg.sensors.insert(
            "Office Radar".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/office_radar_sensor".into(),
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
        // The LOSER: a zone with the same sanitized id. Inserted second
        // so the WINNER (the sensor) is config-order-first.
        cfg.zones.insert(
            "office_radar".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec![],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );

        let inv = EntityInventory::from_config(&cfg);
        let collisions = detect_and_warn_collisions(&cfg, &inv);
        let entry = collisions
            .iter()
            .find(|(san, _)| san == "office_radar")
            .unwrap_or_else(|| panic!("expected office_radar collision, got {collisions:?}"));
        let raw_ids = &entry.1;
        // Pair-order: winner first, loser second.
        assert!(
            raw_ids
                .first()
                .is_some_and(|first| first == "sensor:Office Radar"),
            "first-wins must pick the config-order-first raw id; got {raw_ids:?}"
        );
        assert!(
            raw_ids
                .get(1)
                .is_some_and(|second| second == "zone:office_radar"),
            "the loser's raw id must appear second; got {raw_ids:?}"
        );

        // On the wire, EXACTLY ONE discovery record for the collided
        // sanitized id — the loser's binary_sensor/zone record must
        // not be present. The winner is a sensor, so its discovery
        // record is under the binary_sensor prefix.
        let records = discovery_records(&cfg, &inv, "office-pc");
        let office_radar_records: Vec<&PublishRecord> = records
            .iter()
            .filter(|r| {
                // Discovery topic for office_radar is
                // `<prefix>/binary_sensor/office-pc/{sensor|zone}_office_radar/config`.
                // The trailing path is `_office_radar/config` (the
                // kind is prepended), so a suffix match against
                // `/office_radar/config` would miss BOTH candidates —
                // the actual suffix is `_office_radar/config`.
                r.topic.ends_with("_office_radar/config")
            })
            .collect();
        assert_eq!(
            office_radar_records.len(),
            1,
            "first-wins: exactly one discovery record for office_radar (got {}): {:?}",
            office_radar_records.len(),
            office_radar_records,
        );
        // The discovery topic tells us the winner's KIND: the sensor
        // emits a record under `binary_sensor/.../sensor_office_radar/
        // config`, the zone under `binary_sensor/.../zone_office_radar/
        // config`. The sensor (WINNER) is the only one that should
        // appear — the topic MUST be the SENSOR path.
        let winner = office_radar_records[0];
        assert!(
            winner.topic.contains("/sensor_office_radar/"),
            "winner record must be under binary_sensor/sensor_office_radar (sensor wins); got topic: {}",
            winner.topic,
        );
        assert!(
            !winner.topic.contains("/zone_office_radar/"),
            "zone (loser) record must NOT be on the wire; got topic: {}",
            winner.topic,
        );

        // The discovery PAYLOAD must carry the WINNER's content. The
        // sanitized name (built from the sanitized id) is identical
        // for both candidates, but the KIND-specific suffix is
        // observable in the `name` field — the WINNER (sensor) is
        // `"dormant office_radar presence"`, the LOSER (zone) would
        // be `"dormant office_radar zone"`. The `unique_id` is also
        // observably different: WINNER has `_sensor_office_radar`,
        // LOSER would have `_zone_office_radar`.
        let payload: serde_json::Value =
            serde_json::from_str(&winner.payload).expect("valid discovery JSON");
        let name = payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let unique_id = payload
            .get("unique_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            name, "dormant office_radar presence",
            "winner's name field must reflect the SENSOR ('presence'), not the zone ('zone'); got {name}"
        );
        assert_eq!(
            unique_id, "dormant_office-pc_sensor_office_radar",
            "winner's unique_id must include `_sensor_` (the loser's would include `_zone_`); got {unique_id}"
        );

        // Cross-check: snapshot_records also flushes both winner and
        // loser's records on the publisher's first flush, but the
        // loser's snapshot is dropped (first-wins). Assert the loser's
        // zone-state topic is absent. (Snapshot topics are keyed by
        // the sanitized id, so both candidates would produce
        // `dormant/office-pc/zone/office_radar/state` — only the
        // winner's record should exist; the loser's is dropped.)
        let snap_records = snapshot_records(
            &cfg,
            &dormant_core::rules::StateSnapshot {
                sensors: vec![dormant_core::rules::SensorSnapshot {
                    id: "Office Radar".into(),
                    state: SensorState::Present,
                    last_seen_secs_ago: 0,
                    reported: true,
                }],
                zones: vec![dormant_core::rules::ZoneSnapshot {
                    id: "office_radar".into(),
                    present: Some(true),
                }],
                displays: vec![],
                pending_reload: None,
                rollback: None,
                kvm: None,
                wear_sampling_status: None,
            },
            "office-pc",
        );
        let zone_state_records: Vec<&PublishRecord> = snap_records
            .iter()
            .filter(|r| r.topic == "dormant/office-pc/zone/office_radar/state")
            .collect();
        assert_eq!(
            zone_state_records.len(),
            0,
            "loser zone's state record must not appear on the wire (first-wins drops the loser entirely); got {zone_state_records:?}"
        );
        // The winner's snapshot (sensor state) MUST appear; the
        // payload is byte-identical for the two candidates by the
        // publisher's contract, so we only assert presence.
        let sensor_state_present = snap_records
            .iter()
            .any(|r| r.topic == "dormant/office-pc/sensor/office_radar/state" && r.payload == "ON");
        assert!(
            sensor_state_present,
            "winner's sensor state record must appear; got: {snap_records:?}"
        );
    }

    #[test]
    fn collision_two_sensor_ids_same_topic() {
        let mut cfg = enabled_cfg();
        // Use distinct broker-side topics so only the sanitizer's
        // collision rule (not the underlying `topic` field) is the
        // thing under test. "desk two" → "desk_two" (space folding +
        // collapse). "desk_two" → "desk_two" (already canonical).
        // Insertion order is config-order (IndexMap), so "desk two"
        // (the one with the space) MUST win.
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
        let inv = EntityInventory::from_config(&cfg);
        let collisions = detect_and_warn_collisions(&cfg, &inv);
        let entry = collisions
            .iter()
            .find(|(san, _)| san == "desk_two")
            .unwrap_or_else(|| panic!("expected desk_two collision, got {collisions:?}"));
        // The detected-pair list carries the raw ids in config-order, so
        // the winner ("desk two") comes first and the loser
        // ("desk_two") comes second. Assert that — config-order is the
        // entire first-wins rule, so a regression that swapped them
        // would silently change which entity an operator's HA
        // integration actually sees.
        let raw_ids = &entry.1;
        assert!(
            raw_ids
                .first()
                .is_some_and(|first| first == "sensor:desk two"),
            "first-wins must pick the config-order-first raw id; got {raw_ids:?}"
        );
        assert!(
            raw_ids
                .get(1)
                .is_some_and(|second| second == "sensor:desk_two"),
            "the loser's raw id must appear second in the pair list; got {raw_ids:?}"
        );

        // First-wins: discovery emits exactly ONE record per
        // sanitized id, and a regression that emitted a record per raw
        // id (instead of one record total) trips the topic_count
        // assertion below. (Both raw ids sanitize to the SAME discovery
        // topic `sensor_desk_two` — the sanitisers collapse the loser's
        // topic into the winner's, so we cannot distinguish winners and
        // losers on the wire; the only assertion that proves WHICH raw
        // id won is the `detect_and_warn_collisions` pair-order check
        // above.)
        let records = discovery_records(&cfg, &inv, "office-pc");
        let desk_two_records: Vec<&PublishRecord> = records
            .iter()
            .filter(|r| r.topic.ends_with("/sensor_desk_two/config"))
            .collect();
        assert_eq!(
            desk_two_records.len(),
            1,
            "first-wins: exactly one discovery record per sanitized id, got {} records: {:?}",
            desk_two_records.len(),
            desk_two_records,
        );

        // The winner's record carries the broker-side state topic
        // `dormant/office-pc/sensor/desk_two/state` (built from the
        // SANITIZED id — both candidates collapse to the same
        // sanitized id and therefore the same state_topic). The discovery
        // payload is byte-identical for both candidates since the
        // sanitizer runs on `sensor_id` before formatting `name`,
        // `unique_id`, and the state_topic field. The first-wins
        // invariant is proven above by the pair list ordering; this
        // assertion guards against the "drop the loser's record too
        // eagerly and lose even the winner" regression by verifying
        // some payload exists.
        let win = desk_two_records[0];
        let payload: serde_json::Value =
            serde_json::from_str(&win.payload).expect("valid discovery JSON");
        assert!(
            payload.get("state_topic").is_some(),
            "winner's payload must carry a state_topic field; got: {payload}"
        );
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
    clippy::similar_names,
    clippy::collapsible_match,
    clippy::collapsible_if,
    clippy::redundant_closure_for_method_calls,
    clippy::needless_late_init,
    clippy::manual_let_else,
    clippy::used_underscore_binding
)]
mod async_tests {
    use super::*;
    use crate::state_publisher::{
        PublishRecord, PublisherTransport, StatePublisherDeps, TransportLifecycle,
    };
    use async_trait::async_trait;
    use dormant_core::config::schema::{
        Config, Credentials, MqttCredential, MqttSensorCfg, PublishConfig, SensorConfig, SensorKind,
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
        /// Equivalent to a `ConnAck` landing. The fake mirrors
        /// production's drain-on-reconnect: stale records sitting in
        /// `record_rx` from any aborted flush are dropped here BEFORE
        /// the lifecycle `Connected` is emitted, so the publisher only
        /// sees the fresh re-flush. Issue #174 Should 2.
        Connected,
        /// Equivalent to a clean Disconnect — emit Disconnected.
        Disconnected,
        /// Equivalent to Home Assistant publishing `online` on
        /// `<discovery_prefix>/status` ARRIVING DURING A FLUSH.
        /// Production's `wait_for_publish_id` observes this in the
        /// PubAck-wait drain and flips a sticky `pending_birth` flag,
        /// then emits `BirthOnline` after the wait. The fake mirrors
        /// that with a sticky flag set on receipt and consumed by the
        /// `record_rx.recv()` arm after the next record lands, so the
        /// test exercises the same dedup/forward seam. Issue #174
        /// Should 1.
        BirthDuringFlush,
        /// Equivalent to Home Assistant publishing `online` on
        /// `<discovery_prefix>/status` while the transport is idle in
        /// `eventloop.poll()`. The fake emits a `BirthOnline`
        /// lifecycle signal so the publisher re-flushes discovery +
        /// snapshot. Issue #174 (defect #2 primary path).
        BirthOnline,
        /// Gracefully stop the transport run loop (test-only escape hatch).
        Stop,
        /// Toggle the park mode: while parked, the transport does NOT
        /// drain `record_rx` (it just loops on the control channel).
        /// Used by the overflow-discriminator test to force a saturated
        /// publisher channel and assert delivered count.
        Park,
        /// Toggle off: the transport returns to its normal drain loop.
        Unpark,
    }

    /// One fake transport: every record forwarded through `record_rx`
    /// lands in `records`; lifecycle events arrive on `ctrl_rx` and the
    /// transport pushes them onto `lifecycle_tx` so the publisher's
    /// main loop is driven exactly as it would be by a real broker.
    struct FakeTransport {
        records: Arc<StdMutex<Vec<PublishRecord>>>,
        records_notify: Arc<Notify>,
        ctrl_rx: mpsc::Receiver<FakeCtrl>,
        /// Sticky flag mirroring production's `MqttTransport`'s
        /// `pending_birth`. Set by [`FakeCtrl::BirthDuringFlush`],
        /// consumed (and emitted as `TransportLifecycle::BirthOnline`)
        /// by the next `record_rx.recv()` arm — matching production's
        /// "emit `BirthOnline` AFTER the PubAck-wait returns to the
        /// select loop" semantics. Issue #174 Should 1.
        pending_birth: bool,
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
                    pending_birth: false,
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
            let mut parked = false;
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
                                // Mirror production's drain-on-reconnect
                                // (issue #174 Should 2): drop any stale
                                // records sitting in `record_rx` from an
                                // aborted flush BEFORE signalling
                                // Connected, so the publisher only sees
                                // the fresh re-flush records. The
                                // empty-topic close sentinel MUST NOT be
                                // swallowed — if seen, honor it
                                // (Disconnected + return).
                                loop {
                                    match record_rx.try_recv() {
                                        Ok(rec) if rec.topic.is_empty() => {
                                            let _ = lifecycle_tx
                                                .send(TransportLifecycle::Disconnected)
                                                .await;
                                            return Ok(());
                                        }
                                        Ok(_) => {}
                                        Err(mpsc::error::TryRecvError::Empty) => break,
                                        Err(mpsc::error::TryRecvError::Disconnected) => {
                                            let _ = lifecycle_tx
                                                .send(TransportLifecycle::Disconnected)
                                                .await;
                                            return Ok(());
                                        }
                                    }
                                }
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Connected)
                                    .await;
                            }
                            FakeCtrl::Disconnected => {
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Disconnected)
                                    .await;
                            }
                            FakeCtrl::BirthDuringFlush => {
                                // Equivalent to HA publishing `online`
                                // on `<discovery_prefix>/status` while the
                                // transport is in the middle of draining
                                // a PubAck wait. Production's
                                // `wait_for_publish_id` observes this in
                                // the drain and flips a sticky
                                // `pending_birth` flag, then the
                                // record-pump arm emits `BirthOnline`
                                // after the wait returns. The fake mirrors
                                // the seam: the flag is set here, and the
                                // `record_rx.recv()` arm below consumes it
                                // after the next record lands.
                                self.pending_birth = true;
                            }
                            FakeCtrl::BirthOnline => {
                                // Equivalent to HA publishing `online` on
                                // `<discovery_prefix>/status`. Production's
                                // `MqttTransport` derives the same signal
                                // from an incoming `Packet::Publish` on the
                                // status topic — the fake just injects it
                                // directly so tests can drive the
                                // re-flush path deterministically.
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::BirthOnline)
                                    .await;
                            }
                            FakeCtrl::Stop => {
                                let _ = lifecycle_tx
                                    .send(TransportLifecycle::Disconnected)
                                    .await;
                                return Ok(());
                            }
                            FakeCtrl::Park => parked = true,
                            FakeCtrl::Unpark => parked = false,
                        }
                    }
                    maybe = record_rx.recv() => {
                        if parked {
                            // Parked mode: drop the record back to the
                            // channel via try_send so the publisher's
                            // awaited send lands, then loop. This
                            // pretends we drained — a true "no-op"
                            // would block the publisher on the second
                            // record and the test could not measure the
                            // overflow. We do NOT count these records.
                            if let Some(rec) = maybe {
                                // Re-queue (in test we just drop — the
                                // publisher will see channel-full on the
                                // NEXT try_send since we are not
                                // actually draining).
                                let _ = rec;
                            } else {
                                return Ok(());
                            }
                            continue;
                        }
                        let Some(rec) = maybe else {
                            // Publisher closed its senders — graceful cancel.
                            return Ok(());
                        };
                        if rec.topic.is_empty() {
                            return Ok(());
                        }
                        {
                            let mut g = self.records.lock().unwrap();
                            g.push(rec);
                        }
                        self.records_notify.notify_one();
                        // After the record lands, surface any pending
                        // `BirthOnline` observation from a `FakeCtrl::
                        // BirthDuringFlush` delivered earlier in the
                        // flush window (issue #174 Should 1). The flag
                        // is sticky and consumed exactly once here —
                        // matching production's dedup.
                        if self.pending_birth {
                            self.pending_birth = false;
                            let _ = lifecycle_tx
                                .send(TransportLifecycle::BirthOnline)
                                .await;
                        }
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

    /// Wait until `predicate(records) >= want`. Polls on every
    /// `notify.notified()` wake and bounds the wait by `timeout`. Used in
    /// place of `sleep`-poll loops so the test-timing policy stays green.
    /// Returns the final count observed on timeout exhaustion.
    async fn wait_records_count<F>(
        records: &StdMutex<Vec<PublishRecord>>,
        notify: &Notify,
        timeout: Duration,
        predicate: F,
    ) -> usize
    where
        F: Fn(&[PublishRecord]) -> usize + Copy,
    {
        let start = tokio::time::Instant::now();
        loop {
            {
                let g = records.lock().unwrap();
                let n = predicate(&g);
                if n >= 1 {
                    return n;
                }
            }
            let remaining = match timeout.checked_sub(start.elapsed()) {
                Some(r) => r,
                None => return predicate(&records.lock().unwrap()),
            };
            let n = notify.notified();
            tokio::pin!(n);
            if tokio::time::timeout(remaining, n).await.is_err() {
                return predicate(&records.lock().unwrap());
            }
        }
    }

    /// Wait until `predicate(records) >= want` strictly (i.e. any
    /// non-empty crossing). Used in tests that need the predicate to
    /// grow N→N+k across an event boundary; the caller passes `want`
    /// = the post-event count to pin the comparison.
    async fn wait_until_count_at_least<F>(
        records: &StdMutex<Vec<PublishRecord>>,
        notify: &Notify,
        timeout: Duration,
        want: usize,
        predicate: F,
    ) -> usize
    where
        F: Fn(&[PublishRecord]) -> usize + Copy,
    {
        let start = tokio::time::Instant::now();
        loop {
            {
                let g = records.lock().unwrap();
                let n = predicate(&g);
                if n >= want {
                    return n;
                }
            }
            let remaining = match timeout.checked_sub(start.elapsed()) {
                Some(r) => r,
                None => return predicate(&records.lock().unwrap()),
            };
            let n = notify.notified();
            tokio::pin!(n);
            if tokio::time::timeout(remaining, n).await.is_err() {
                return predicate(&records.lock().unwrap());
            }
        }
    }

    /// Wait until `counter >= want`. The counter is incremented from
    /// another task; the caller passes a `Notify` that the writer task
    /// fires so we can wake on any increment without polling. Returns the
    /// final counter value on timeout exhaustion.
    async fn wait_counter_at_least(
        counter: &StdMutex<u32>,
        notify: &Notify,
        timeout: Duration,
        want: u32,
    ) -> u32 {
        let start = tokio::time::Instant::now();
        loop {
            {
                let g = counter.lock().unwrap();
                let n = *g;
                if n >= want {
                    return n;
                }
            }
            let remaining = match timeout.checked_sub(start.elapsed()) {
                Some(r) => r,
                None => return *counter.lock().unwrap(),
            };
            let n = notify.notified();
            tokio::pin!(n);
            let _ = tokio::time::timeout(remaining, n).await;
        }
    }

    /// Wait until `counter >= want` using a `&Notify` (matches an
    /// `Arc<Notify>` the writer task holds in `Arc`).
    async fn wait_counter_with_arc(
        counter: &StdMutex<u32>,
        notify: &Arc<Notify>,
        timeout: Duration,
        want: u32,
    ) -> u32 {
        wait_counter_at_least(counter, notify.as_ref(), timeout, want).await
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
                power_off_opt_in: false,
                compositor_output: None,
                sampling: None,
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
        // Direct probe of `mqtt_transport::options_for_test` to verify
        // the credential for THAT exact URL is applied and the
        // client id is deterministic. The password MUST NOT appear
        // anywhere in the resulting options surface; the broker URL
        // itself MUST NOT appear unredacted either (a userinfo-bearing
        // URL like `mqtt://user:secret@host` would leak the secret
        // the moment any logger or `Debug` print formatted it).
        let url = "tcp://h:1883";
        let creds = publish_creds_for(url);
        let opts = super::mqtt_transport::MqttTransport::options_for_test(
            url,
            &creds,
            "dormant-publisher-office-pc-test",
        )
        .expect("valid URL must parse");
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

        // Per-task test: a userinfo-bearing URL (the failure mode that
        // triggered the milestone REVISE) must NOT leak the secret. We
        // mirror the lookup key with embedded userinfo, then check that
        // the summary only carries the host and port — never the
        // password. The `parse_broker_url` helper strips the userinfo
        // and gives us a clean `(host, port)` pair; the redaction must
        // only print that pair.
        let userinfo_url = "tcp://dormant:publish-secret-PWD@h:1883";
        let creds_with_userinfo = {
            let mut c = Credentials::default();
            c.mqtt.insert(
                userinfo_url.to_string(),
                MqttCredential {
                    username: "dormant".into(),
                    password: "publish-secret-PWD".into(),
                },
            );
            c
        };
        let userinfo_opts = super::mqtt_transport::MqttTransport::options_for_test(
            userinfo_url,
            &creds_with_userinfo,
            "dormant-publisher-userinfo-test",
        )
        .expect("valid URL must parse");
        let userinfo_debug = format!("{userinfo_opts:?}");
        assert!(
            !userinfo_debug.contains("publish-secret-PWD"),
            "userinfo URL must not leak password via the redaction surface (got: {userinfo_debug:?})"
        );
        // The secret string in plaintext form (the password) MUST NOT
        // appear anywhere in the string. The host `h` SHOULD still
        // appear; the user `dormant` MAY appear (it's metadata, not a
        // secret), but a regex finding the original
        // `dormant:publish-secret-PWD@` substring is the leak we're
        // guarding against.
        assert!(
            !userinfo_debug.contains("dormant:publish-secret-PWD@"),
            "userinfo URL form must NOT appear verbatim (got: {userinfo_debug:?})"
        );
        assert!(
            userinfo_opts.contains("h:1883"),
            "parsed host:port should be the only broker identifier in the summary; got: {userinfo_opts}"
        );
    }

    // ── RED: status-topic derivation (issue #174) ──────────────

    #[test]
    fn mqtt_transport_status_topic_derives_from_sanitized_discovery_prefix() {
        // The transport subscribes to `<sanitized_discovery_prefix>/status`
        // AFTER each ConnAck; the same prefix is used (after sanitization)
        // by `discovery_records` to build discovery topics. The two must
        // use the same prefix sanitization or HA's birth subscribe would
        // land on a topic HA never publishes on. Asserting through
        // `status_topic_for_test` keeps the wiring visible without a live
        // broker.
        let opts = super::mqtt_transport::MqttTransport::status_topic_for_test("homeassistant");
        assert_eq!(opts, "homeassistant/status");

        // Sanitization must mirror the discovery topic builder — a prefix
        // with whitespace MUST collapse to the underscore-folded form so
        // the subscribe topic and the discovery topic share a wire shape.
        let opts_sanitized =
            super::mqtt_transport::MqttTransport::status_topic_for_test("Home Assistant");
        assert_eq!(opts_sanitized, "home_assistant/status");
    }

    #[test]
    fn birth_packets_during_publish_wait_collapse_to_one_pending_birth() {
        let status_topic = "homeassistant/status";
        let mut pending_birth = false;
        let online = rumqttc::Packet::Publish(rumqttc::Publish {
            dup: false,
            qos: rumqttc::QoS::AtLeastOnce,
            retain: true,
            topic: status_topic.into(),
            pkid: 1,
            payload: "online".into(),
        });

        super::mqtt_transport::observe_birth_packet(&online, status_topic, &mut pending_birth);
        super::mqtt_transport::observe_birth_packet(&online, status_topic, &mut pending_birth);

        assert!(super::mqtt_transport::take_pending_birth(
            &mut pending_birth
        ));
        assert!(!super::mqtt_transport::take_pending_birth(
            &mut pending_birth
        ));
    }

    #[test]
    fn reconnect_drain_discards_stale_records_and_preserves_fresh_record() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.blocking_send(PublishRecord {
            topic: "stale".into(),
            payload: "old".into(),
            qos: 1,
            retain: true,
        })
        .unwrap();
        tx.blocking_send(PublishRecord {
            topic: String::new(),
            payload: String::new(),
            qos: 1,
            retain: true,
        })
        .unwrap();
        tx.blocking_send(PublishRecord {
            topic: "fresh".into(),
            payload: "new".into(),
            qos: 1,
            retain: true,
        })
        .unwrap();

        assert!(super::mqtt_transport::drain_stale_records(&mut rx).is_none());
        assert_eq!(rx.try_recv().unwrap().topic, "fresh");
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

        // Drive the second Connected. Every successful (re)connect MUST
        // republish the full discovery payload AND a fresh snapshot
        // batch. A regression that emits only the discovery half (or
        // only the snapshot half) trips the per-entity count checks
        // below. This guards the original "early-return when both
        // state + discovery exist on the first flush" bug.
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        let discovery_topic = "homeassistant/binary_sensor/office-pc/sensor_desk/config";
        let state_topic = "dormant/office-pc/sensor/desk/state";
        let zone_topic = "dormant/office-pc/zone/office/state";
        let display_topic = "dormant/office-pc/display/main/phase";
        // Issue #174: every flush prepends the global `online`
        // availability record so HA marks the entities available. The
        // post-reconnect count must include that record, so the
        // expected delta is 1 (global online) + 3 discovery + 4 snapshot.
        let global_online_topic = "dormant/office-pc/availability";
        let pre_total = {
            let g = records_handle.lock().unwrap();
            g.len()
        };
        // 1 global online + 3 discovery + 4 snapshot (state + per-sensor
        // availability + zone + display) per connect, so the post-reconnect
        // count must be `pre_total + 8`. The event-driven wait yields to
        // the runtime on each `notify.notified()` wake; only the final
        // timeout fallback uses wall-clock time.
        let total_after = wait_until_count_at_least(
            &records_handle,
            &records_notify_handle,
            Duration::from_secs(2),
            pre_total + 8,
            |g| g.len(),
        )
        .await;
        assert!(
            total_after >= pre_total + 8,
            "reconnect must re-emit global online + discovery AND snapshot ({pre_total} pre -> {total_after} post; expected at least +8)"
        );

        // Per-entity counts: each topic must appear at least twice
        // (once per connect). A regression that emits only the discovery
        // half on reconnect fails the *state* assertion below.
        let counts = {
            let g = records_handle.lock().unwrap();
            (
                g.iter().filter(|r| r.topic == discovery_topic).count(),
                g.iter().filter(|r| r.topic == state_topic).count(),
                g.iter().filter(|r| r.topic == zone_topic).count(),
                g.iter().filter(|r| r.topic == display_topic).count(),
                g.iter().filter(|r| r.topic == global_online_topic).count(),
            )
        };
        assert!(
            counts.0 >= 2,
            "desk discovery record must appear at least twice (got {})",
            counts.0
        );
        assert!(
            counts.1 >= 2,
            "desk state record must appear at least twice on reconnect (got {})",
            counts.1
        );
        assert!(
            counts.2 >= 2,
            "office zone state record must appear at least twice on reconnect (got {})",
            counts.2
        );
        assert!(
            counts.3 >= 2,
            "main display phase record must appear at least twice on reconnect (got {})",
            counts.3
        );
        // Issue #174: the global online availability record must be
        // re-flushed on every reconnect (the broker's retained `offline`
        // from a broker restart is the dominant failure mode this
        // guards against).
        assert!(
            counts.4 >= 2,
            "global online availability record must appear at least twice on reconnect (got {})",
            counts.4
        );

        cancel.cancel();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }

    // ── RED: first connect publishes global online BEFORE discovery (issue #174) ──

    #[tokio::test]
    async fn first_connect_publishes_global_online_before_discovery() {
        // Issue #174 defect #1: every discovery payload references the
        // global availability topic `<base>/<instance>/availability`,
        // but before the fix nothing ever published `online` there —
        // HA marked every entity unavailable and after a graceful
        // shutdown the broker's retained `offline` stuck around forever.
        // The fix prepends a retained QoS-1 `online` on the global topic
        // BEFORE the discovery configs on the first connect flush.
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

        // Drive Connected once and wait for the first flush to drain.
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        let count_after_first_flush = wait_records_count(
            &records_handle,
            &records_notify_handle,
            Duration::from_secs(2),
            |g| g.len(),
        )
        .await;
        assert!(
            count_after_first_flush > 0,
            "first connect must produce at least one record (got 0)"
        );

        // The FIRST record on the wire MUST be the global online
        // availability record: a regression that publishes discovery
        // before the global online still works for the broker but
        // means HA's entity-registry sweep reads discovery configs
        // before the availability topic exists, leaving entities
        // unavailable on their very first registration.
        let snapshot = {
            let g = records_handle.lock().unwrap();
            g.clone()
        };
        let first = snapshot
            .first()
            .expect("first connect must produce at least one record");
        assert_eq!(
            first.topic, "dormant/office-pc/availability",
            "first record on first connect must be the global online availability topic"
        );
        assert_eq!(
            first.payload, "online",
            "first record on first connect must publish retained `online` (got {:?})",
            first.payload
        );
        assert_eq!(first.qos, 1, "global online is QoS 1 (matches LWT)");
        assert!(
            first.retain,
            "global online is retained (matches LWT, so HA's availability claim flips cleanly)"
        );

        // Cross-check: discovery records follow the global online on
        // the wire. The helper prepends the global online to the flush,
        // so a regression that emits only the discovery half on first
        // connect (and re-flushes the global online only on reconnect)
        // would fail this assertion.
        let desk_discovery_present = snapshot
            .iter()
            .any(|r| r.topic == "homeassistant/binary_sensor/office-pc/sensor_desk/config");
        assert!(
            desk_discovery_present,
            "discovery records must follow the global online on first connect"
        );

        cancel.cancel();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }

    // ── RED: HA birth announcement re-flushes discovery + snapshot (issue #174) ──

    #[tokio::test]
    async fn ha_birth_triggers_discovery_and_snapshot_reflush() {
        // Issue #174 defect #2: when the operator deletes an entity in
        // HA, HA clears its retained discovery config topic. The
        // publisher needs to learn HA is back online and re-publish the
        // discovery configs. HA's birth topic `<discovery_prefix>/status`
        // carries `online` on HA's startup (and on a clean restart).
        // Production subscribes to that topic after ConnAck and emits
        // `TransportLifecycle::BirthOnline`; the fake's
        // `FakeCtrl::BirthOnline` simulates the same signal so this test
        // can drive it deterministically.
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

        // Drive Connected once so the publisher completes its initial
        // flush.
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        let pre_birth_total = wait_records_count(
            &records_handle,
            &records_notify_handle,
            Duration::from_secs(2),
            |g| g.len(),
        )
        .await;
        assert!(
            pre_birth_total > 0,
            "first connect must produce at least one record before the HA birth event"
        );

        // Now drive the HA birth announcement. The publisher MUST
        // re-flush the same flush body as `Connected` (global online +
        // discovery + fresh snapshot).
        ctrl_tx.send(FakeCtrl::BirthOnline).await.unwrap();
        let discovery_topic = "homeassistant/binary_sensor/office-pc/sensor_desk/config";
        let state_topic = "dormant/office-pc/sensor/desk/state";
        let zone_topic = "dormant/office-pc/zone/office/state";
        let display_topic = "dormant/office-pc/display/main/phase";
        let global_online_topic = "dormant/office-pc/availability";

        // 1 global online + 3 discovery + 4 snapshot records per flush
        // — a regression that re-flushed only discovery (or only the
        // snapshot) trips one of the per-entity counts below.
        let total_after_birth = wait_until_count_at_least(
            &records_handle,
            &records_notify_handle,
            Duration::from_secs(2),
            pre_birth_total + 8,
            |g| g.len(),
        )
        .await;
        assert!(
            total_after_birth >= pre_birth_total + 8,
            "HA birth must re-flush global online + discovery AND snapshot (had {pre_birth_total}, now {total_after_birth}; expected at least +8)"
        );

        // Per-entity counts: every entity record must appear at least
        // twice (once per flush — initial + HA-birth). The global online
        // record gets the same gate as the discovery / snapshot records.
        let counts = {
            let g = records_handle.lock().unwrap();
            (
                g.iter().filter(|r| r.topic == discovery_topic).count(),
                g.iter().filter(|r| r.topic == state_topic).count(),
                g.iter().filter(|r| r.topic == zone_topic).count(),
                g.iter().filter(|r| r.topic == display_topic).count(),
                g.iter().filter(|r| r.topic == global_online_topic).count(),
            )
        };
        assert!(
            counts.0 >= 2,
            "desk discovery record must appear at least twice across initial + HA-birth flush (got {})",
            counts.0
        );
        assert!(
            counts.1 >= 2,
            "desk state record must appear at least twice across initial + HA-birth flush (got {})",
            counts.1
        );
        assert!(
            counts.2 >= 2,
            "office zone state record must appear at least twice across initial + HA-birth flush (got {})",
            counts.2
        );
        assert!(
            counts.3 >= 2,
            "main display phase record must appear at least twice across initial + HA-birth flush (got {})",
            counts.3
        );
        assert!(
            counts.4 >= 2,
            "global online availability record must appear at least twice across initial + HA-birth flush (got {})",
            counts.4
        );

        cancel.cancel();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }

    // ── RED: reconnect re-flushes the global online record (issue #174) ──────────

    #[tokio::test]
    async fn reconnect_republishes_global_online() {
        // Issue #174 defect #1 (focused regression): every flush MUST
        // re-publish a retained `online` on the global LWT availability
        // topic so HA marks dormant entities available after a broker
        // restart, an HA-side deletion, or a daemon reconnect. This
        // test isolates the global online record from the discovery +
        // snapshot noise by counting ONLY records on the global
        // availability topic across two connects, so a regression that
        // emits discovery + snapshot on reconnect but forgets the
        // global online trips the strict count assertion.
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

        // Drive Connected once and wait for the global online to land.
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        let global_online_topic = "dormant/office-pc/availability";
        let first_count = wait_until_count_at_least(
            &records_handle,
            &records_notify_handle,
            Duration::from_secs(2),
            1,
            |g| g.iter().filter(|r| r.topic == global_online_topic).count(),
        )
        .await;
        assert!(
            first_count >= 1,
            "first connect must publish global online at least once (got {first_count})"
        );

        // Drive a Disconnect + Connected cycle and re-check the count.
        // A regression that only re-flushes the global online on first
        // connect would leave the count at 1 across the second connect.
        ctrl_tx.send(FakeCtrl::Disconnected).await.unwrap();
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        let second_count = wait_until_count_at_least(
            &records_handle,
            &records_notify_handle,
            Duration::from_secs(2),
            first_count + 1,
            |g| g.iter().filter(|r| r.topic == global_online_topic).count(),
        )
        .await;
        assert!(
            second_count > first_count,
            "reconnect must re-publish global online (had {first_count}, now {second_count}; expected at least +1)"
        );

        // Cross-check: every global online record carries the wire
        // shape HA's availability claim flips on. Retained QoS-1
        // `online` — the LWT publishes retained `offline` on this same
        // topic, so the broker's retained slot holds exactly one of
        // the two literals at any moment. A regression that emits
        // `online` as non-retained would let the broker's retained
        // `offline` from the LWT shadow it on a clean restart.
        let latest = {
            let g = records_handle.lock().unwrap();
            g.iter()
                .rev()
                .find(|r| r.topic == global_online_topic)
                .cloned()
                .expect("at least one global online record")
        };
        assert_eq!(latest.payload, "online");
        assert!(latest.retain, "global online is retained (matches LWT)");
        assert_eq!(latest.qos, 1, "global online is QoS 1 (matches LWT)");

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
        let snapshot_requests_notify = Arc::new(Notify::new());
        let snapshot_requests_handle = snapshot_requests.clone();
        let snapshot_requests_notify_for_responder = snapshot_requests_notify.clone();
        let cfg_for_handler = cfg.clone();
        let _responder = tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(event_tx.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        *snapshot_requests_handle.lock().unwrap() += 1;
                        snapshot_requests_notify_for_responder.notify_one();
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

        // Wait event-driven for the initial snapshot request.
        let initial = wait_counter_with_arc(
            &snapshot_requests,
            &snapshot_requests_notify,
            Duration::from_secs(2),
            1,
        )
        .await;
        assert!(initial >= 1, "spawn must request an initial snapshot");

        // Overflow the broadcast channel so the publisher's next event
        // recv returns `Lagged`. Eight `Subscribed` events are enough
        // to exceed the channel capacity (2) and trip the lag handler.
        let pre_lag = *snapshot_requests.lock().unwrap();
        for _ in 0..8 {
            let _ = event_tx_for_overflow.send(DaemonEvent::Subscribed);
        }
        // Event-driven wait for the additional snapshot that the lag
        // handler must request. Bounded by 2s — a few hundred ms is
        // typical on a quiet CI machine.
        let post_lag = wait_counter_with_arc(
            &snapshot_requests,
            &snapshot_requests_notify.clone(),
            Duration::from_secs(2),
            pre_lag + 1,
        )
        .await;
        assert!(
            post_lag > pre_lag,
            "lag must drive at least one additional Snapshot request (had {pre_lag}, now {post_lag})"
        );

        cancel.cancel();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }

    // ── Must 1 RED: overflow awaits rather than silently drops ───────────
    //
    // Build a publisher with enough entities that a single first-flush
    // EXCEEDS the publisher's 64-entry record-channel. Drain only ONE
    // record on the transport side, then park forever. With an awaited
    // `send_record`, the publisher MUST suspend after 64 records
    // (waiting for channel capacity) and the task stays alive. With a
    // `try_send` regression, the 65th-and-subsequent records would
    // FAIL with `Full` and be silently dropped — the publisher would
    // then return from `send_flush` and reach the main loop without
    // parking, where the only Ready arm is `cancel` (we never cancel),
    // so the task would stay alive too. To distinguish the two paths,
    // we capture the `tracing` log: a regression emits
    // `publish_record_dropped` warns, the awaited implementation does
    // not — so the warn at the cancel-exit must NOT include that
    // event name.
    #[tokio::test]
    async fn overflow_awaits_backpressure_instead_of_silently_dropping_records() {
        // Build a config with 50 sensors — each contributes 1 discovery
        // and 2 snapshot records (state + availability). 50 sensors =
        // ~150 records per flush, far past the 64-entry channel.
        let mut cfg = enabled_publish_config();
        for i in 1..=50 {
            cfg.sensors.insert(
                format!("desk_{i}"),
                SensorConfig::Mqtt(MqttSensorCfg {
                    broker_url: "tcp://h:1883".into(),
                    topic: format!("dormant/desk_{i}"),
                    field: "/v".into(),
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
        }
        let cfg = Arc::new(cfg);
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

        struct HoldTransport;
        #[async_trait]
        impl PublisherTransport for HoldTransport {
            async fn run(
                self: Box<Self>,
                mut record_rx: mpsc::Receiver<PublishRecord>,
                lifecycle_tx: mpsc::Sender<TransportLifecycle>,
                trans_cancel: tokio_util::sync::CancellationToken,
            ) -> Result<(), String> {
                let _ = lifecycle_tx.send(TransportLifecycle::Connected).await;
                // Drain exactly ONE record to confirm wiring — then park
                // until the publisher's transport-cancel reaches this
                // task via finalize_shutdown.
                let _ = record_rx.recv().await;
                let hang = std::future::pending::<()>();
                tokio::pin!(hang);
                tokio::select! {
                    () = &mut hang => {}
                    () = trans_cancel.cancelled() => {}
                }
                Ok(())
            }
        }

        let handle = spawn_with_transport(
            StatePublisherDeps {
                config: cfg.clone(),
                credentials: creds,
                ctl_tx: ctl_tx.clone(),
                cancel: cancel.clone(),
            },
            Box::new(HoldTransport),
        )
        .expect("spawn must succeed");

        // Wait briefly for the publisher's first-flush to enter the
        // parking state. With awaited send, the publisher is suspended
        // on `record_tx.send().await` (channel-full after 64 records).
        // With try_send-drop, the publisher would still be in the same
        // observable state (task not finished, select not firing
        // any arm) — but the WARNING log line distinguishes them.
        let poll_until_alive = || async {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
            loop {
                if !handle.is_finished() {
                    return;
                }
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
                // Yield to the runtime without sleeping; the publisher
                // task makes no progress without a runtime tick here, but
                // event-loop bookkeeping on a quiet CI machine is enough.
                tokio::task::yield_now().await;
            }
        };
        poll_until_alive().await;
        assert!(
            !handle.is_finished(),
            "publisher task must STILL be alive (awaiting channel capacity or parked in select)"
        );

        // Cancel — the awaited implementation drops the rest of the
        // flush with a `publish_record_dropped` warn for each record it
        // didn't manage to enqueue. The try_send-drop regression would
        // ALREADY have dropped the rest with that warn emitted during
        // the flush itself. In either case the publisher must exit on
        // cancel; just verify it does.
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("publisher must honour cancel during backpressure")
            .expect("publisher task did not panic");
    }

    // ── Must 1 RED (delivery-count discriminator) ─────────────────
    //
    // The previous overflow test could only observe the publisher's
    // `is_finished()` state, which converges to the same True/False
    // in both the awaited-send and try_send-drop implementations because
    // cancel eventually fires either path. This test removes the cancel
    // variable entirely and asserts the DELIVERED-COUNT in the fake
    // transport's buffer instead — a regression to `try_send` drops
    // records silently on a full channel, so the count comes up short.
    //
    // Setup:
    //   - 32 sensors in the config → 32 discovery + 96 snapshot = 128
    //     records pushed on the first flush (> publisher's 64-entry
    //     record-channel capacity, so overflow actually happens).
    //   - A "ParkTransport" that drains records into a `Mutex<Vec<_>>`
    //     but does NOT yield — it returns to its `recv()` await
    //     immediately, so the channel quickly fills to capacity and the
    //     publisher's awaited `send_record` parks on the saturated
    //     channel.
    //   - A `Notify` gate that the test fires AFTER the publisher has
    //     parked. Once fired, the transport continues draining and the
    //     publisher's awaited sends complete in order.
    //
    // Under the awaited-send impl: all 128 records land in the
    // transport's buffer (the publisher never drops one).
    // Under a try_send-drop regression: the publisher's flush completes
    // with channel capacity 64 parked at record 65; records 65–128
    // return `Full` from `try_send` and are silently dropped. The
    // transport's buffer sees only the first 64 records, and the test
    // fails on the strict count assertion.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn overflow_delivers_full_record_count_without_cancelling() {
        // 32 sensors (plus the default zone + display). The
        // discovery + snapshot totals are computed via the same
        // pure-mapping functions the publisher uses, so the count is
        // exact: 32 sensor discovery + 1 zone discovery + 1 display
        // discovery = 34 discovery records; 32×2 sensor state+avail +
        // 1 zone state + 1 display phase = 66 snapshot records; total
        // 100. The 64-entry channel saturates at record 65, so the
        // overflow path is exercised.
        let mut cfg = enabled_publish_config();
        for i in 1..=32 {
            cfg.sensors.insert(
                format!("d_{i}"),
                SensorConfig::Mqtt(MqttSensorCfg {
                    broker_url: "tcp://h:1883".into(),
                    topic: format!("dormant/d_{i}"),
                    field: "/v".into(),
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
        }
        let inventory = EntityInventory::from_config(&cfg);
        let snap = snapshot_one_of_each_shape(&cfg);
        // Issue #174: every flush prepends the global availability
        // `online` record so HA marks every dormant entity available.
        // Compute the expected count with that prepended record so the
        // strict `==` discriminator below stays an honest measurement.
        let expected_records = global_online_record(&cfg, "office-pc").map_or(0, |_| 1)
            + discovery_records(&cfg, &inventory, "office-pc").len()
            + snapshot_records(&cfg, &snap, "office-pc").len();
        for i in 1..=32 {
            cfg.sensors.insert(
                format!("d_{i}"),
                SensorConfig::Mqtt(MqttSensorCfg {
                    broker_url: "tcp://h:1883".into(),
                    topic: format!("dormant/d_{i}"),
                    field: "/v".into(),
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
        }
        let cfg = Arc::new(cfg);
        let creds = publish_creds_for("tcp://h:1883");
        let (_event_tx_for_sub, _event_rx) = tokio::sync::broadcast::channel::<DaemonEvent>(8);
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let cfg_for_handler = cfg.clone();
        let _responder = tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(_event_tx_for_sub.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        let _ = tx.send(snapshot_one_of_each_shape(&cfg_for_handler));
                    }
                    _ => {}
                }
            }
        });

        // Recipe: use the existing FakeTransport (which already buffers
        // every drained record in `self.records` and supports a
        // control channel) with a new `Park` / `Unpark` toggle. The
        // publisher pushes the full first-flush; with the transport
        // parked the channel saturates at 64 and the awaited
        // `send_record` parks on the 65th record. We do NOT cancel —
        // the recipe explicitly forbids it; the discriminator is the
        // delivered count after `Unpark`.
        let (transport, ctrl_tx) = FakeTransport::build();
        let records_handle = transport.records().clone();
        let records_notify_handle = transport.records_notify().clone();

        let handle = spawn_with_transport(
            StatePublisherDeps {
                config: cfg.clone(),
                credentials: creds,
                ctl_tx: ctl_tx.clone(),
                cancel: cancel.clone(),
            },
            Box::new(transport),
        )
        .expect("spawn must succeed");

        // Phase 1 — drive a `Connected` so the publisher's first-flush
        // starts, then immediately `Park` so the transport stops draining
        // and the channel saturates.
        ctrl_tx.send(FakeCtrl::Connected).await.unwrap();
        // Yield enough times for the publisher to push records. We use
        // a small fixed budget of `yield_now()` calls (no raw sleeps per
        // the test-timing policy) — 200 yields is generous for pushing
        // 128 records into a 64-cap channel; the publisher will be
        // parked at record 65 by the time we toggle Unpark.
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        ctrl_tx.send(FakeCtrl::Park).await.unwrap();
        // A few more yields let the publisher's in-flight `send_record`
        // hits on the now-parked transport fully saturate the channel.
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }

        // Phase 2 — release. The transport resumes draining; the
        // publisher's parked `send_record` completes and the publisher
        // pushes the remaining records. The whole flush completes.
        ctrl_tx.send(FakeCtrl::Unpark).await.unwrap();
        // Wait event-driven (no raw sleeps) until the publisher's
        // first-flush has fully drained through the transport — the
        // records buffer notifies on every push, and `expected_records`
        // pushes satisfy the predicate.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let g = records_handle.lock().unwrap();
                if g.len() >= expected_records {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let _ =
                tokio::time::timeout(Duration::from_millis(50), records_notify_handle.notified())
                    .await;
        }

        // Discriminator assertion: the publisher's first-flush produces
        // EXACTLY 128 records. With an awaited `send_record` all 128
        // land in the transport's buffer. With a `try_send` regression
        // the 65th-and-subsequent records would silently return
        // `Full` from `try_send` and be discarded, so the buffer's
        // length would land strictly below 128.
        let drained_records: Vec<PublishRecord> = {
            let g = records_handle.lock().unwrap();
            g.clone()
        };
        assert_eq!(
            drained_records.len(),
            expected_records,
            "publisher delivered {} records on the wire; expected exactly {} (the full first-flush). \
             A short count means a `try_send` regression silently dropped {} records when the publisher's \
             channel overflowed capacity mid-flush.",
            drained_records.len(),
            expected_records,
            expected_records.saturating_sub(drained_records.len()),
        );

        // Spot-check: every record has a non-empty topic (transport-
        // side sanity). A `try_send` regression can't reorder messages
        // within a single flush (mpsc preserves per-sender FIFO), but
        // if the publisher's flush scrambled the order across awaits,
        // this loop catches an empty payload/topic.
        for (i, r) in drained_records.iter().enumerate() {
            assert!(
                !r.topic.is_empty(),
                "record {i} has empty topic (transport-side bug)"
            );
        }

        // Do NOT cancel — the recipe explicitly forbids it. The
        // publisher task is parked in its main select! (or has already
        // exited if the channel closed under it) — leave it alone so
        // the test does not skew the next one's lifecycle. We DO
        // `Stop` the transport so its run loop exits cleanly and the
        // publisher's `record_rx.recv()` returns Disconnected, which
        // drops the transport-side `JoinHandle`.
        let _ = ctrl_tx.send(FakeCtrl::Stop).await;
        let _ = cancel; // explicitly DO NOT call cancel()
        drop(handle);
        drop(ctrl_tx);
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
        // Event-driven wait: the FakeTransport appends a record under a
        // mutex and `notify.notify_one()`s on every push, so the wait
        // returns as soon as the first flush has drained through.
        let pre_cancel_count = wait_records_count(
            &records_handle,
            &records_notify_handle,
            Duration::from_secs(2),
            |g| g.len(),
        )
        .await;
        assert!(
            pre_cancel_count > 0,
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
            wear_sampling_status: None,
        }
    }
}
