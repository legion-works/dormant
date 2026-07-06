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
//! The daemon's runner creates a fresh broadcast channel on every successful
//! reload.  When the old channel closes
//! ([`broadcast::error::RecvError::Closed`]), the handler does **not** drop
//! the WebSocket — it re-issues [`ControlMsg::SubscribeEvents`] through
//! `ctl_tx` (which `forward_ctl` routes to the new generation).  The browser
//! never has to reconnect.
//!
//! On successful reload a synthetic `config_reloaded` frame is sent so
//! the frontend can re-fetch `/api/config`.
//!
//! ### Validation rejection — deferred-close state machine
//!
//! A normal [`ReloadOutcome::Rejected`] keeps the old generation alive, so
//! the existing `ev_rx` is still valid and continues to deliver any
//! backlogged or in-flight events.  Touching `ev_rx` on `Rejected`
//! (draining its buffer or replacing it) would silently lose events, so
//! the handler does NEITHER: it only arms `resubscribe_on_close` and
//! returns to the select! loop.  The receiver stays in place and the
//! stream stays lossless regardless of how many events buffered during
//! the reject-subscribe round-trip on either side.
//!
//! The only reason a `Rejected` outcome ever triggers a resubscribe is the
//! rare teardown-rebuild path where `dormantd`'s `rebuild_old` actually
//! drops the old broadcast before publishing `Rejected` — in that case
//! the old `ev_rx` reports `Closed` shortly after `Rejected` was observed.
//! The `Closed` branch then sees the flag and resubscribes into the new
//! generation.  On a true normal reject, `Closed` never arrives and the
//! flag is harmless.

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
#[allow(clippy::too_many_lines)]
async fn stream_events(socket: WebSocket, state: &WebState) -> Result<(), Error> {
    let ctl_tx = state.inner.ctl_tx.clone();
    let cancel = state.inner.cancel.clone();
    let mut reload_rx = state.inner.reload_rx.resubscribe();

    let (mut tx, mut rx) = socket.split();

    let mut events: Option<broadcast::Receiver<DaemonEvent>> =
        Some(resubscribe_events(&ctl_tx).await?);

    // When the daemon tears down the old generation, rebuilds, then
    // publishes Rejected, the `select!` may see Rejected BEFORE the old
    // broadcast's Closed lands on the event receiver.  This flag arms a
    // deferred resubscribe: when Closed DOES arrive (on the next
    // iteration), we resubscribe instead of going idle.  On a normal
    // validation reject (old gen still alive) the flag is set but the
    // receiver never closes → no-op, no event loss.
    let mut resubscribe_on_close = false;

    loop {
        if let Some(ref mut ev_rx) = events {
            tokio::select! {
                inbound = rx.next() => {
                    match inbound {
                        Some(Ok(Message::Close(_)) | Err(_)) | None => return Ok(()),
                        _ => {}
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
                            if resubscribe_on_close {
                                // A reload signal was seen earlier; the
                                // teardown just caught up.  Subscribe to
                                // the new generation.
                                resubscribe_on_close = false;
                                events = resubscribe_events(&ctl_tx).await.ok();
                            } else {
                                events = None;
                            }
                        }
                    }
                }

                outcome = reload_rx.recv() => {
                    match outcome {
                        Ok(ReloadOutcome::Reloaded) => {
                            resubscribe_on_close = false;
                            let frame = serde_json::to_string(&DaemonEvent::ConfigReloaded)
                                .unwrap_or_default();
                            let _ = tx.send(Message::Text(frame)).await;
                            events = resubscribe_events(&ctl_tx).await.ok();
                        }
                        Ok(ReloadOutcome::Rejected(_)) => {
                            // Normal validation reject: the old
                            // generation is still alive.  Do NOT
                            // touch `ev_rx` — draining or replacing
                            // it loses any event buffered during the
                            // reject-subscribe round-trip.  Arm the
                            // deferred-close flag so the teardown
                            // case (broadcast closes shortly after
                            // Rejected is published) is handled by
                            // the `Closed` branch; a true normal
                            // reject never closes the receiver and
                            // the flag is harmless.
                            resubscribe_on_close = true;
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
                            resubscribe_on_close = false;
                            let frame = serde_json::to_string(&DaemonEvent::ConfigReloaded)
                                .unwrap_or_default();
                            let _ = tx.send(Message::Text(frame)).await;
                            events = resubscribe_events(&ctl_tx).await.ok();
                        }
                        Ok(ReloadOutcome::Rejected(_)) => {
                            // Events channel is already closed — try
                            // subscribing to whatever generation is
                            // currently running.
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
        // Held to keep the reload broadcast's Sender alive for the
        // whole test: `stream_events` calls `state.inner.reload_rx
        // .resubscribe()` per WS connection, which requires the channel
        // to still have a Sender.  Dropping it earlier closes the
        // channel and forces every handler out via `Closed`.
        #[allow(dead_code)]
        reload_tx: broadcast::Sender<ReloadOutcome>,
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
            reload_tx,
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
    /// promptly.  After close, the handler's broadcast receiver is dropped,
    /// so `event_tx.receiver_count()` drops within a short timeout.
    #[tokio::test]
    async fn idle_client_close_exits_handler() {
        let cancel = CancellationToken::new();
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let (_reload_tx, reload_rx) = broadcast::channel::<ReloadOutcome>(16);
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(64);

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

        // Wait for the subscription to land BEFORE closing, otherwise the
        // receiver_count check below could observe 0 (pre-subscribe) and
        // pass without the handler ever having run.
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => {
                    panic!("subscription was not established within 2s");
                }
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if event_tx.receiver_count() == 1 {
                        break;
                    }
                }
            }
        }

        // Close from the client side; the handler's `rx.next()` arm fires
        // and `stream_events` returns.
        let _ = ws.close(None).await;

        // After the handler exits, its broadcast receiver is dropped and
        // `receiver_count()` returns to 0.  Polling for that transition —
        // not for `<= 1` — is what proves the handler task actually exited.
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => {
                    panic!("handler did not exit within 2s");
                }
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if event_tx.receiver_count() == 0 {
                        return;
                    }
                }
            }
        }
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
    #[allow(clippy::too_many_lines)]
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
        let (drop_gen1_tx, mut drop_gen1_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            let mut phase: u8 = 1;
            // gen1_clone is the only sender handle; the Option owns it.
            // When gen1 is set to None, the broadcast closes.
            let mut gen1: Option<broadcast::Sender<DaemonEvent>> = Some(gen1_clone);
            loop {
                if gen1.is_some() {
                    tokio::select! {
                        new_phase = phase_rx.recv() => {
                            if let Some(p) = new_phase { phase = p; }
                        }
                        _res = &mut drop_gen1_rx => { gen1 = None; }
                        msg = ctl_rx.recv() => {
                            match msg {
                                Some(ControlMsg::SubscribeEvents(tx)) => {
                                    if let Some(ref g1) = gen1 {
                                        if phase == 2 {
                                            let _ = tx.send(gen2_clone.subscribe());
                                        } else {
                                            let _ = tx.send(g1.subscribe());
                                        }
                                    }
                                }
                                Some(_) => {}
                                None => break,
                            }
                        }
                    }
                } else {
                    tokio::select! {
                        new_phase = phase_rx.recv() => {
                            if let Some(p) = new_phase { phase = p; }
                        }
                        msg = ctl_rx.recv() => {
                            match msg {
                                Some(ControlMsg::SubscribeEvents(tx)) => {
                                    if phase == 2 {
                                        let _ = tx.send(gen2_clone.subscribe());
                                    }
                                    // gen1 is None → gen1 broadcast closed;
                                    // no gen1 subscription possible.
                                }
                                Some(_) => {}
                                None => break,
                            }
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
        // Tell the engine to drop its gen1 handle so the broadcast closes.
        let _ = drop_gen1_tx.send(());
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

    /// MUST 2 — teardown-reject: a rebuild that drops the old generation's
    /// broadcast then publishes `Rejected` must NOT leave the WebSocket
    /// dead.  The handler observes `Rejected` first (arming
    /// `resubscribe_on_close`), then the old `ev_rx` reports `Closed`,
    /// which routes the existing `Closed` branch into a fresh subscription
    /// against the new generation.
    #[allow(clippy::too_many_lines)]
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

        let gen1_clone = gen1_tx.clone();
        let gen2_clone = gen2_tx.clone();
        let (drop_gen1_tx, mut drop_gen1_rx) = oneshot::channel::<()>();
        let (resub_signal_tx, mut resub_signal_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            // gen1 starts holding the only engine-side sender.  Dropping
            // it (driven from the test) closes the gen1 broadcast.
            let mut gen1: Option<broadcast::Sender<DaemonEvent>> = Some(gen1_clone);
            loop {
                if gen1.is_some() {
                    tokio::select! {
                        _ = &mut drop_gen1_rx => { gen1 = None; }
                        msg = ctl_rx.recv() => {
                            match msg {
                                Some(ControlMsg::SubscribeEvents(tx)) => {
                                    if let Some(ref g1) = gen1 {
                                        let _ = tx.send(g1.subscribe());
                                    }
                                }
                                Some(_) => {}
                                None => break,
                            }
                        }
                    }
                } else {
                    let msg = ctl_rx.recv().await;
                    match msg {
                        Some(ControlMsg::SubscribeEvents(tx)) => {
                            let _ = tx.send(gen2_clone.subscribe());
                            // Signal the test that the handler is now
                            // subscribed to gen2, so subsequent
                            // gen2_tx.send() reaches the live receiver.
                            let _ = resub_signal_tx.send(()).await;
                        }
                        Some(_) => {}
                        None => break,
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

        // Baseline on gen1.
        let _ = gen1_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("gen1-pre".to_string()),
            state: SensorState::Present,
        });
        let f1 = recv_json(&mut ws).await;
        assert_eq!(f1["event"], "sensor_changed");
        assert_eq!(f1["sensor"], "gen1-pre");

        // Tear the old broadcast down and publish Rejected.  In a real
        // daemon, `rebuild_old` may close the broadcast before the
        // validation outcome is published; this is the path that the
        // deferred-close flag is meant to recover.
        let _ = drop_gen1_tx.send(());
        drop(gen1_tx);
        let _ = reload_tx.send(ReloadOutcome::Rejected("rebuild rejected".into()));

        // Wait until the engine has handed the handler a fresh gen2
        // subscription BEFORE we publish the post-teardown event —
        // otherwise the gen2 broadcast's ring buffer would hold an event
        // no subscriber has ever seen (broadcast receivers only deliver
        // messages sent after they subscribe).
        let _ = resub_signal_rx.recv().await;

        // A gen2 event arrives.  With the flag+Closed→resubscribe path,
        // the handler's WS connection survives the teardown and delivers
        // gen2 events.
        let _ = gen2_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("gen2-post".to_string()),
            present: true,
            cause: dormant_core::types::SensorId("gen2".to_string()),
        });

        let f2 = recv_json(&mut ws).await;
        assert_eq!(f2["event"], "zone_changed");
        assert_eq!(f2["zone"], "gen2-post");

        let _ = ws.close(None).await;
    }

    /// A normal validation reject (old gen still alive) must keep the
    /// live receiver — no event loss on buffered OR in-flight events,
    /// stream continues on gen1 with all events in order.
    ///
    /// Determinism: the engine task delays every resubscribe (second
    /// `SubscribeEvents` onward) by 50ms, so any events sent to the broadcast
    /// during that window land in the OLD receiver's buffer. With the
    /// `drain-one-then-resubscribe` pattern, replacing the receiver loses
    /// those events; setting only `resubscribe_on_close` preserves them.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn rejected_normal_no_event_loss() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let cancel = CancellationToken::new();
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let (reload_tx, reload_rx) = broadcast::channel::<ReloadOutcome>(16);
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(64);

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

        let first_subscribe = StdArc::new(AtomicBool::new(true));
        let first_subscribe_clone = first_subscribe.clone();
        let event_tx_for_engine = event_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::SubscribeEvents(tx) = msg {
                    if !first_subscribe_clone.swap(false, Ordering::SeqCst) {
                        // Simulate the rebuild path: every resubscribe
                        // takes 50ms. With the buggy drain-then-resubscribe
                        // Rejected handler, events sent in this window are
                        // dropped when the OLD receiver is replaced.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let _ = tx.send(event_tx_for_engine.subscribe());
                }
            }
        });

        let app = Router::new()
            .route("/api/events", get(ws_events))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel_clone.cancelled().await })
                .await
                .ok();
        });

        let url = format!("ws://{addr}/api/events");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Baseline event on the established subscription — confirms the
        // handler is live and reading from ev_rx.
        let _ = event_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("pre-reject".to_string()),
            state: SensorState::Present,
        });
        let f1 = recv_json(&mut ws).await;
        assert_eq!(f1["event"], "sensor_changed");
        assert_eq!(f1["sensor"], "pre-reject");

        // Normal validation Reject: old generation stays alive. With the
        // deferred-close fix, the handler arms the flag and keeps streaming
        // on ev_rx. With the buggy drain-then-resubscribe, it replaces
        // ev_rx during the 50ms engine delay, dropping anything buffered.
        let _ = reload_tx.send(ReloadOutcome::Rejected("bad config".into()));

        // Three events sent back-to-back on the SAME broadcast. With the
        // buggy handler, all three land in the OLD receiver's buffer and
        // are dropped when the OLD receiver is replaced mid-resubscribe.
        // With the fix, ev_rx is never replaced and forwards all three.
        let _ = event_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("z1".to_string()),
            present: true,
            cause: dormant_core::types::SensorId("c1".to_string()),
        });
        let _ = event_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("z2".to_string()),
            present: true,
            cause: dormant_core::types::SensorId("c2".to_string()),
        });
        let _ = event_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("z3".to_string()),
            present: true,
            cause: dormant_core::types::SensorId("c3".to_string()),
        });

        // ALL THREE must arrive on the same WS, in send order, with no gap.
        let e1 = recv_json(&mut ws).await;
        assert_eq!(e1["event"], "zone_changed");
        assert_eq!(e1["zone"], "z1");
        let e2 = recv_json(&mut ws).await;
        assert_eq!(e2["event"], "zone_changed");
        assert_eq!(e2["zone"], "z2");
        let e3 = recv_json(&mut ws).await;
        assert_eq!(e3["event"], "zone_changed");
        assert_eq!(e3["zone"], "z3");

        let _ = ws.close(None).await;
    }
}
