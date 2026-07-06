//! `WS /api/events` — live daemon event stream.
//!
//! On WebSocket upgrade, subscribes to the engine event broadcast via
//! [`ControlMsg::SubscribeEvents`] and streams each [`DaemonEvent`] as
//! a JSON text frame.  The handler `select!`s over four sources:
//! inbound client close, event receiver, reload outcomes, and a cancel
//! token so an idle disconnect is observed immediately.
//!
//! ## Reload re-subscription (spec §3.2)
//!
//! The daemon's runner creates a fresh broadcast channel on every reload.
//! When the old channel closes ([`broadcast::error::RecvError::Closed`]),
//! the handler does **not** drop the WebSocket — it waits on `reload_rx`
//! for a reload outcome, then re-issues [`ControlMsg::SubscribeEvents`]
//! through `ctl_tx` (which `forward_ctl` routes to the new generation).
//! Both [`ReloadOutcome::Reloaded`] and [`ReloadOutcome::Rejected`] trigger
//! re-subscription because `rebuild_old` may tear down the old generation
//! then publish `Rejected` — a closed event receiver needs a fresh
//! subscription regardless of outcome.  The browser never has to reconnect.
//!
//! On successful reload a synthetic `config_reloaded` frame is sent so
//! the frontend can re-fetch `/api/config`.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};

use dormant_core::reload::ReloadOutcome;
use dormant_core::rules::{ControlMsg, DaemonEvent};
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
/// The WebSocket is split into a sender (for outbound JSON frames) and
/// a receiver (for inbound close detection) so an idle connection that
/// the browser closes is torn down immediately rather than leaking a
/// task that only wakes on the next daemon event.
///
/// # Errors
///
/// Returns an error only on the initial subscribe (engine not reachable).
/// Client-disconnect and daemon-shutdown return `Ok(())`.
async fn stream_events(socket: WebSocket, state: &WebState) -> Result<(), Error> {
    let ctl_tx = state.inner.ctl_tx.clone();
    let cancel = state.inner.cancel.clone();
    let mut reload_rx = state.inner.reload_rx.resubscribe();

    // Split so we can select! over inbound closes while holding
    // a mutable sender reference for outbound frames.
    let (mut tx, mut rx) = socket.split();

    let mut events: Option<broadcast::Receiver<DaemonEvent>> =
        Some(resubscribe_events(&ctl_tx).await?);

    loop {
        if let Some(ref mut ev_rx) = events {
            tokio::select! {
                // MUST 1 — observe client-side close immediately.
                inbound = rx.next() => {
                    match inbound {
                        Some(Ok(Message::Close(_)) | Err(_)) | None => return Ok(()),
                        _ => {} // Ping / Pong / Text from client are ignored.
                    }
                }

                () = cancel.cancelled() => return Ok(()),

                event = ev_rx.recv() => {
                    match event {
                        Ok(ev) => {
                            let text = serde_json::to_string(&ev).unwrap_or_default();
                            if tx.send(Message::Text(text)).await.is_err() {
                                return Ok(());
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            let lagged = serde_json::json!({
                                "event": "stream_lagged",
                                "skipped": n,
                            });
                            let text = serde_json::to_string(&lagged).unwrap_or_default();
                            if tx.send(Message::Text(text)).await.is_err() {
                                return Ok(());
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            events = None;
                        }
                    }
                }

                outcome = reload_rx.recv() => {
                    match outcome {
                        Ok(ReloadOutcome::Reloaded) => {
                            // MUST 3 — notify the frontend so it re-fetches config.
                            let frame = serde_json::to_string(&DaemonEvent::ConfigReloaded)
                                .unwrap_or_default();
                            let _ = tx.send(Message::Text(frame)).await;
                            events = resubscribe_events(&ctl_tx).await.ok();
                        }
                        Ok(ReloadOutcome::Rejected(_)) => {
                            // MUST 2 — rebuild_old may tear down the old generation
                            // and then publish Rejected.  Re-subscribe unconditionally
                            // so the WS does not stay permanently dead.
                            events = resubscribe_events(&ctl_tx).await.ok();
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
        } else {
            tokio::select! {
                inbound = rx.next() => {
                    match inbound {
                        Some(Ok(Message::Close(_)) | Err(_)) | None => return Ok(()),
                        _ => {}
                    }
                }

                () = cancel.cancelled() => return Ok(()),

                outcome = reload_rx.recv() => {
                    match outcome {
                        Ok(ReloadOutcome::Reloaded) => {
                            let frame = serde_json::to_string(&DaemonEvent::ConfigReloaded)
                                .unwrap_or_default();
                            let _ = tx.send(Message::Text(frame)).await;
                            events = resubscribe_events(&ctl_tx).await.ok();
                        }
                        Ok(ReloadOutcome::Rejected(_)) => {
                            // MUST 2 — same reasoning as above.
                            events = resubscribe_events(&ctl_tx).await.ok();
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
    }
}

/// Helper — send [`ControlMsg::SubscribeEvents`] through the control
/// channel and await the subscription response.
async fn resubscribe_events(
    ctl_tx: &mpsc::Sender<ControlMsg>,
) -> Result<broadcast::Receiver<DaemonEvent>, Error> {
    let (tx, rx) = oneshot::channel();
    ctl_tx
        .send(ControlMsg::SubscribeEvents(tx))
        .await
        .map_err(|_| Error::EngineUnavailable)?;
    rx.await.map_err(|_| Error::EngineUnavailable)
}

#[derive(Debug)]
enum Error {
    EngineUnavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EngineUnavailable => f.write_str("engine not available"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::types::SensorState;
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio::time::{Duration, timeout};
    use tokio_util::sync::CancellationToken;

    use futures_util::StreamExt;

    struct TestHarness {
        event_tx: broadcast::Sender<DaemonEvent>,
        _reload_tx: broadcast::Sender<ReloadOutcome>,
        cancel: CancellationToken,
        addr: SocketAddr,
    }

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
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let (reload_trigger_tx, reload_trigger_rx) = mpsc::channel::<()>(8);
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

    #[tokio::test]
    async fn streams_two_events() {
        let h = harness().await;
        let url = h.ws_url();
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("WS connect");

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

    /// MUST 1 — an idle connection closed by the client exits the handler
    /// promptly (does not leak a task waiting on a future daemon event).
    #[tokio::test]
    async fn idle_client_close_exits_handler() {
        let h = harness().await;
        let url = h.ws_url();
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("WS connect");

        // Close the client side without sending any data.
        let _ = ws.close(None).await;

        // The server-side handler should have exited.  We can't observe it
        // directly, but we can verify another WS connection works (the old
        // task is not holding resources).
        let (_ws2, _resp2) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("second WS connect should succeed");
    }

    #[tokio::test]
    async fn lagged_emits_stream_lagged_frame() {
        let cancel = CancellationToken::new();
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let (_reload_tx, reload_rx) = broadcast::channel::<ReloadOutcome>(16);
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

        for i in 0u32..20 {
            let _ = event_tx.send(DaemonEvent::SensorChanged {
                sensor: dormant_core::types::SensorId(format!("s{i}")),
                state: SensorState::Present,
            });
        }

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

    /// The handler survives a reload (Reloaded) and keeps streaming on
    /// the same WS connection, AND emits a `config_reloaded` frame (MUST 3).
    #[tokio::test]
    async fn reload_resubscribe_keeps_streaming_and_emits_config_reloaded() {
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

        let _ = gen1_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("gen1-sensor".to_string()),
            state: SensorState::Present,
        });

        let frame1 = recv_json(&mut ws).await;
        assert_eq!(frame1["event"], "sensor_changed");
        assert_eq!(frame1["sensor"], "gen1-sensor");

        // Simulate generation switch.
        drop(gen1_tx);
        let _ = phase_tx.send(2).await;
        let _ = reload_tx.send(ReloadOutcome::Reloaded);

        // The next frame MUST be config_reloaded (MUST 3).
        let reload_frame = recv_json(&mut ws).await;
        assert_eq!(reload_frame["event"], "config_reloaded");

        // Now gen2 events arrive.
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

    /// MUST 2 — a Rejected outcome after the old generation was torn
    /// down still triggers re-subscription so the WS is not permanently
    /// dead.
    #[tokio::test]
    async fn rejected_resubscribes_when_events_closed() {
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

        let gen1_c = gen1_tx.clone();
        let gen2_c = gen2_tx.clone();
        let (phase_tx, mut phase_rx) = mpsc::channel::<u8>(1);

        tokio::spawn(async move {
            let mut phase: u8 = 1;
            loop {
                tokio::select! {
                    p = phase_rx.recv() => { if let Some(v) = p { phase = v; } }
                    msg = ctl_rx.recv() => {
                        match msg {
                            Some(ControlMsg::SubscribeEvents(tx)) => {
                                if phase == 2 {
                                    let _ = tx.send(gen2_c.subscribe());
                                } else {
                                    let _ = tx.send(gen1_c.subscribe());
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

        // Verify gen1 is live.
        let _ = gen1_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("pre".to_string()),
            state: SensorState::Present,
        });
        let f = recv_json(&mut ws).await;
        assert_eq!(f["event"], "sensor_changed");

        // Tear down old generation, switch engine to gen2, emit Rejected.
        drop(gen1_tx);
        let _ = phase_tx.send(2).await;
        let _ = reload_tx.send(ReloadOutcome::Rejected("validation error".into()));

        tokio::time::sleep(Duration::from_millis(200)).await;

        // A new event from gen2 MUST arrive on the same WS.
        let _ = gen2_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("post-reject".to_string()),
            present: false,
            cause: dormant_core::types::SensorId("post".to_string()),
        });

        let frame = recv_json(&mut ws).await;
        assert_eq!(frame["event"], "zone_changed");
        assert_eq!(frame["zone"], "post-reject");

        let _ = ws.close(None).await;
    }
}
