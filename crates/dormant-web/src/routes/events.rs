//! `WS /api/events` — live daemon event stream.
//!
//! On WebSocket upgrade, subscribes to the engine event broadcast via
//! [`ControlMsg::SubscribeEvents`] and streams each [`DaemonEvent`] as
//! a JSON text frame.  The handler `select!`s over both its event receiver
//! **and** the daemon-level `reload_rx` so it can re-subscribe when a
//! config reload spawns a new engine generation.
//!
//! ## Reload re-subscription (spec §3.2)
//!
//! The daemon's runner creates a fresh broadcast channel on every reload.
//! When the old channel closes ([`broadcast::error::RecvError::Closed`]),
//! the handler does **not** drop the WebSocket — it waits on `reload_rx`
//! for a [`ReloadOutcome::Reloaded`], then re-issues
//! [`ControlMsg::SubscribeEvents`] through `ctl_tx` (which
//! `forward_ctl` routes to the new generation).  The browser
//! never has to reconnect.
//!
//! This deliberately differs from the IPC server which closes on
//! `Closed` — the web UI is interactive and must survive reloads.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;

use dormant_core::reload::ReloadOutcome;
use dormant_core::rules::ControlMsg;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::WebState;

/// Axum handler — accepts the WebSocket upgrade and delegates to
/// [`stream_events`] inside the upgrade callback.
pub(crate) async fn ws_events(
    State(state): State<WebState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| async move {
        if let Err(e) = stream_events(socket, &state).await {
            tracing::debug!(event = "ws_events_error", error = %e);
        }
    })
}

/// Core streaming loop — subscribe, stream, re-subscribe on reload.
///
/// # Errors
///
/// Returns an error only on the initial subscribe (engine not reachable).
/// Client-disconnect and daemon-shutdown return `Ok(())`.
async fn stream_events(mut socket: WebSocket, state: &WebState) -> Result<(), Error> {
    let ctl_tx = state.inner.ctl_tx.clone();
    // resubscribe() returns a fresh Receiver that sees all future
    // broadcasts — cheap (no new allocation beyond the channel book-keeping).
    let mut reload_rx = state.inner.reload_rx.resubscribe();

    // Initial subscribe.
    let mut events: Option<broadcast::Receiver<dormant_core::rules::DaemonEvent>> =
        Some(resubscribe_events(&ctl_tx).await?);

    loop {
        if let Some(ref mut ev_rx) = events {
            // Engine broadcast is open — select over both event stream
            // and reload outcomes.  No MutexGuard is held across any
            // `.await` point in this branch.
            tokio::select! {
                event = ev_rx.recv() => {
                    match event {
                        Ok(ev) => {
                            let text = serde_json::to_string(&ev).unwrap_or_default();
                            if socket.send(Message::Text(text)).await.is_err() {
                                return Ok(()); // client disconnected
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Mirror ipc.rs:320-326 — synthetic frame, continue.
                            let lagged = serde_json::json!({
                                "event": "stream_lagged",
                                "skipped": n,
                            });
                            let text = serde_json::to_string(&lagged).unwrap_or_default();
                            if socket.send(Message::Text(text)).await.is_err() {
                                return Ok(());
                            }
                            // Receiver is still valid after Lagged.
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Old engine generation dropped — fall through to the
                            // reload-only path below.
                            events = None;
                        }
                    }
                }
                outcome = reload_rx.recv() => {
                    match outcome {
                        Ok(ReloadOutcome::Reloaded) => {
                            // Re-subscribe to the new generation's event stream.
                            events = resubscribe_events(&ctl_tx).await.ok();
                        }
                        Ok(ReloadOutcome::Rejected(_)) => {
                            // Old generation still running; nothing to do.
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // reload_rx lost messages — re-subscribe to the bus.
                            // This is rare (low-volume channel) but must not spin.
                            reload_rx = state.inner.reload_rx.resubscribe();
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Daemon-level reload bus closed → daemon is shutting down.
                            return Ok(());
                        }
                    }
                }
            }
        } else {
            // Engine broadcast is closed — wait ONLY on reload_rx to avoid a
            // tight spin on Closed.  The browser's WS stays open.
            match reload_rx.recv().await {
                Ok(ReloadOutcome::Reloaded) => {
                    events = resubscribe_events(&ctl_tx).await.ok();
                }
                Ok(ReloadOutcome::Rejected(_)) => {
                    // Still nothing to do; keep waiting.
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    reload_rx = state.inner.reload_rx.resubscribe();
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Ok(());
                }
            }
        }
    }
}

/// Helper — send [`ControlMsg::SubscribeEvents`] through the control
/// channel and await the subscription response.
///
/// Returns the new [`broadcast::Receiver`] on success.
async fn resubscribe_events(
    ctl_tx: &mpsc::Sender<ControlMsg>,
) -> Result<broadcast::Receiver<dormant_core::rules::DaemonEvent>, Error> {
    let (tx, rx) = oneshot::channel();
    ctl_tx
        .send(ControlMsg::SubscribeEvents(tx))
        .await
        .map_err(|_| Error::EngineUnavailable)?;
    rx.await.map_err(|_| Error::EngineUnavailable)
}

/// Error surface for the event-stream handler.
#[derive(Debug)]
enum Error {
    /// The engine is not reachable for the initial subscribe.
    EngineUnavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EngineUnavailable => f.write_str("engine not available"),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::DaemonEvent;
    use dormant_core::types::SensorState;
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio::time::{Duration, timeout};
    use tokio_util::sync::CancellationToken;

    use futures_util::StreamExt;

    // ── Test infrastructure ──────────────────────────────────────────────

    /// All the channels and handles needed for a WS events test.
    struct TestHarness {
        /// Push events to the WS client through this channel.
        event_tx: broadcast::Sender<DaemonEvent>,
        /// Signal a reload to the WS handler.
        _reload_tx: broadcast::Sender<ReloadOutcome>,
        /// Cancel the server task.
        cancel: CancellationToken,
        /// The server's bound address.
        addr: SocketAddr,
    }

    /// Build a test server with a controllable engine event broadcast and
    /// reload bus.  The engine task responds to `SubscribeEvents` by
    /// providing a subscriber to `event_tx`.  On drop, the server is
    /// cancelled.
    async fn harness() -> TestHarness {
        let cancel = CancellationToken::new();
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let (reload_tx, reload_rx) = broadcast::channel::<ReloadOutcome>(16);
        let (event_tx, _event_rx) = broadcast::channel::<DaemonEvent>(64);

        let config = Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);
        // Keep watch senders alive.
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let (reload_trigger_tx, reload_trigger_rx) = mpsc::channel::<()>(8);
        // Only the sender is used; keep the receiver alive to avoid
        // a spurious close of the Sender.
        std::mem::forget(reload_trigger_rx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let state = WebState::new(crate::state::WebStateInner {
            ctl_tx,
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
        });

        let event_tx_for_engine = event_tx.clone();

        // Engine task: respond to SubscribeEvents with our controlled
        // broadcast.
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::SubscribeEvents(tx) = msg {
                    let _ = tx.send(event_tx_for_engine.subscribe());
                }
                // Ignore other control messages.
            }
        });

        let app = Router::new()
            .route("/api/events", get(ws_events))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .expect("bind for test harness");
        let addr = listener.local_addr().unwrap();

        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel_clone.cancelled().await })
                .await
                .ok();
        });

        TestHarness {
            event_tx,
            _reload_tx: reload_tx,
            cancel,
            addr,
        }
    }

    impl TestHarness {
        fn ws_url(&self) -> String {
            format!("ws://{}/api/events", self.addr)
        }
    }

    impl Drop for TestHarness {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    /// Read one JSON text frame from the WS, with a 2s timeout.
    async fn recv_json(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> serde_json::Value {
        let msg = timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timeout waiting for WS frame")
            .expect("WS stream ended")
            .expect("WS error");
        let text = msg.to_text().expect("expected text frame");
        serde_json::from_str(text).expect("invalid JSON frame")
    }

    // ── Test: stream >=2 events ──────────────────────────────────────────

    #[tokio::test]
    async fn streams_two_events() {
        let h = harness().await;
        let url = h.ws_url();
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("WS connect");

        // Push two events into the broadcast.
        let _ = h.event_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("s1".to_string()),
            state: SensorState::Present,
        });
        let _ = h.event_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("z1".to_string()),
            present: true,
            cause: dormant_core::types::SensorId("s1".to_string()),
        });

        let frame1 = recv_json(&mut ws).await;
        assert_eq!(frame1["event"], "sensor_changed");
        assert_eq!(frame1["sensor"], "s1");
        assert_eq!(frame1["state"], "present");

        let frame2 = recv_json(&mut ws).await;
        assert_eq!(frame2["event"], "zone_changed");
        assert_eq!(frame2["zone"], "z1");

        let _ = ws.close(None).await;
    }

    // ── Test: Lagged → stream_lagged frame ──────────────────────────────

    #[tokio::test]
    async fn lagged_emits_stream_lagged_frame() {
        // Use a harness with a TINY event broadcast so lag is easy.
        let cancel = CancellationToken::new();
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let (_reload_tx, reload_rx) = broadcast::channel::<ReloadOutcome>(16);
        // Capacity 2 — filling it forces lag on a slow consumer.
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(2);

        let config = Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let (reload_trigger_tx, reload_trigger_rx) = mpsc::channel::<()>(8);
        // Only the sender is used; keep the receiver alive to avoid
        // a spurious close of the Sender.
        std::mem::forget(reload_trigger_rx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let state = WebState::new(crate::state::WebStateInner {
            ctl_tx,
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
        });

        let event_tx_for_engine = event_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::SubscribeEvents(tx) = msg {
                    let _ = tx.send(event_tx_for_engine.subscribe());
                }
            }
        });

        let app = Router::new()
            .route("/api/events", get(ws_events))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
                .ok();
        });

        let url = format!("ws://{addr}/api/events");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Flood the tiny channel — the WS handler can't consume fast enough.
        for i in 0u32..20 {
            let _ = event_tx.send(DaemonEvent::SensorChanged {
                sensor: dormant_core::types::SensorId(format!("s{i}")),
                state: SensorState::Present,
            });
        }

        // Collect frames until we see stream_lagged or 3s passes.
        let mut saw_lagged = false;
        let deadline = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                msg = ws.next() => {
                    let msg = msg.expect("stream ended").expect("WS error");
                    let text = msg.to_text().expect("text frame");
                    let v: serde_json::Value = text.parse().expect("JSON");
                    if v["event"] == "stream_lagged" {
                        assert!(v["skipped"].as_u64().is_some(),
                            "stream_lagged must include 'skipped' count");
                        saw_lagged = true;
                        break;
                    }
                }
                () = &mut deadline => {
                    break;
                }
            }
        }

        assert!(saw_lagged, "expected a stream_lagged frame within 3s");

        let _ = ws.close(None).await;
    }

    // ── Test: reload re-subscribe ────────────────────────────────────────

    #[tokio::test]
    async fn reload_resubscribe_keeps_streaming() {
        let cancel = CancellationToken::new();
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let (reload_tx, reload_rx) = broadcast::channel::<ReloadOutcome>(16);
        let (gen1_tx, _gen1_rx) = broadcast::channel::<DaemonEvent>(64);
        let (gen2_tx, _gen2_rx) = broadcast::channel::<DaemonEvent>(64);

        let config = Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let (reload_trigger_tx, reload_trigger_rx) = mpsc::channel::<()>(8);
        // Only the sender is used; keep the receiver alive to avoid
        // a spurious close of the Sender.
        std::mem::forget(reload_trigger_rx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let state = WebState::new(crate::state::WebStateInner {
            ctl_tx,
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
        });

        // Multi-generation engine.
        // Phase 1: gen1_tx is active → SubscribeEvents → gen1 subscriber.
        // Phase 2: after gen1_tx is dropped + reload signal, the NEXT
        //   SubscribeEvents → gen2 subscriber.
        let gen1_clone = gen1_tx.clone();
        let gen2_clone = gen2_tx.clone();
        let (phase_tx, mut phase_rx) = mpsc::channel::<u8>(1);

        tokio::spawn(async move {
            let mut phase: u8 = 1;
            loop {
                tokio::select! {
                    new_phase = phase_rx.recv() => {
                        if let Some(p) = new_phase { phase = p; }
                    }
                    msg = ctl_rx.recv() => {
                        match msg {
                            Some(ControlMsg::SubscribeEvents(tx)) => {
                                if phase == 2 {
                                    let _ = tx.send(gen2_clone.subscribe());
                                } else {
                                    let _ = tx.send(gen1_clone.subscribe());
                                }
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                }
            }
        });

        let app = Router::new()
            .route("/api/events", get(ws_events))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
                .ok();
        });

        let url = format!("ws://{addr}/api/events");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Step 1: push an event on gen1, verify it arrives.
        let _ = gen1_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("gen1-sensor".to_string()),
            state: SensorState::Present,
        });

        let frame1 = recv_json(&mut ws).await;
        assert_eq!(frame1["event"], "sensor_changed");
        assert_eq!(frame1["sensor"], "gen1-sensor");

        // Step 2: simulate generation switch.
        // Drop gen1 broadcast → the WS handler's event receiver gets Closed.
        drop(gen1_tx);
        // Tell the engine to serve gen2 on the next SubscribeEvents.
        let _ = phase_tx.send(2).await;
        // Signal Reloaded on the reload bus.
        let _ = reload_tx.send(ReloadOutcome::Reloaded);

        // Small settle time for the handler to process reload + re-subscribe.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Step 3: push an event on gen2, verify it arrives on the SAME
        // connection.
        let _ = gen2_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("gen2-zone".to_string()),
            present: false,
            cause: dormant_core::types::SensorId("gen2".to_string()),
        });

        let frame2 = recv_json(&mut ws).await;
        assert_eq!(frame2["event"], "zone_changed");
        assert_eq!(frame2["zone"], "gen2-zone");

        let _ = ws.close(None).await;
    }
}
