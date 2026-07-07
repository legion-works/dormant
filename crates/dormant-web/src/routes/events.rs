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
//! ### State machine — `Closed` is the teardown signal, period
//!
//! The handler has exactly two stable states — `events = Some(ev_rx)`
//! (streaming) and `events = None` (resubscribe failed; waiting for
//! shutdown).  There is no flag tracking prior reload outcomes because
//! every interleaving of `ReloadOutcome` and `ev_rx.recv()` resolves the
//! same way:
//!
//! - A normal validation [`ReloadOutcome::Rejected`] NEVER closes
//!   `ev_rx` (the old generation stays alive), so observing a
//!   `Closed` on the event receiver is unambiguous evidence that the
//!   generation was torn down.  `Rejected` therefore does nothing to
//!   `ev_rx` and there is no state for a sticky flag to leak across
//!   reload cycles.
//! - `Reloaded` only emits the `config_reloaded` frame.  Resubscription
//!   is driven by `Closed` on `ev_rx` (the canonical teardown
//!   signal), so the handler never races a `Reloaded` arm against a
//!   not-yet-installed new generation by issuing its own
//!   `SubscribeEvents` at that moment.
//! - `Closed` on `ev_rx` always resubscribes into whatever generation
//!   the engine currently reports.  If the swap hasn't completed yet
//!   the engine replies with a receiver on the closed broadcast and
//!   we immediately try again — the engine's swap is bounded, so this
//!   converges without hot-spinning.

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
                            // Unambiguous teardown: a normal Reject
                            // never closes the broadcast, so any
                            // `Closed` here means the active
                            // generation was torn down.  Resubscribe
                            // into the engine's current gen; if its
                            // swap hasn't completed we'll get a
                            // closed receiver back and immediately
                            // try again — bounded by the engine swap.
                            events = resubscribe_events(&ctl_tx).await.ok();
                        }
                    }
                }

                outcome = reload_rx.recv() => {
                    match outcome {
                        Ok(ReloadOutcome::Reloaded) => {
                            // Only job is announcing the reload to the
                            // frontend.  Resubscription is driven by
                            // the `Closed` branch above so the handler
                            // never issues `SubscribeEvents` against a
                            // not-yet-installed generation (the
                            // canonical teardown signal lands first).
                            let frame = serde_json::to_string(&DaemonEvent::ConfigReloaded)
                                .unwrap_or_default();
                            let _ = tx.send(Message::Text(frame)).await;
                        }
                        Ok(ReloadOutcome::Rejected(detail)) => {
                            // Emit a rejected frame so the frontend can
                            // show the validation failure.  The events
                            // receiver is still valid (normal reject
                            // never tears down the generation), so no
                            // resubscribe is needed here.
                            let frame = serde_json::json!({
                                "event": "config_reload_rejected",
                                "detail": detail,
                            });
                            let text =
                                serde_json::to_string(&frame).unwrap_or_default();
                            let _ = tx.send(Message::Text(text)).await;
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
                        Ok(ReloadOutcome::Rejected(detail)) => {
                            // Emit the reject frame even when the
                            // events channel lagged — the frontend
                            // must see the validation failure.
                            let frame = serde_json::json!({
                                "event": "config_reload_rejected",
                                "detail": detail,
                            });
                            let text =
                                serde_json::to_string(&frame).unwrap_or_default();
                            let _ = tx.send(Message::Text(text)).await;
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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
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

        // Drain the config_reload_rejected frame emitted by the recovery
        // arm BEFORE the resubscribe completed.
        let reject_drain = recv_json(&mut ws).await;
        assert_eq!(reject_drain["event"], "config_reload_rejected");
        assert_eq!(reject_drain["detail"], "rebuild rejected");

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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
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
        // The Rejected arm now emits a config_reload_rejected frame first
        // — drain it before checking the event stream.
        let reject_drain = recv_json(&mut ws).await;
        assert_eq!(reject_drain["event"], "config_reload_rejected");
        assert_eq!(reject_drain["detail"], "bad config");

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

    /// Interleaving E (reviewer's stale-flag regression): after a normal
    /// validation reject the handler must NOT keep any sticky state that
    /// could fire spuriously on a future reload.  The new scenario:
    ///
    /// 1. Subscribe.
    /// 2. Receive a normal `Rejected` (gen1 alive).
    /// 3. Daemon tears down gen1 (broadcast `Closed` arrives at the handler).
    /// 4. `Reloaded` is published AFTER the old broadcast closes.
    ///
    /// The Reloaded arm in any sound design resubscribes into the new
    /// generation.  A buggy design whose `Reloaded` arm calls
    /// `resubscribe_events` *and* keeps a receiver-buffer from a prior
    /// `Closed`-triggered resubscribe will DROP that buffer (and any
    /// events queued in it) when the second resubscribe replaces it.
    /// The engine sleeps 50 ms before responding to every
    /// `SubscribeEvents` so the timing of
    ///   gen2-event-sent → Reloaded-arm's-resubscribe-completes
    /// is deterministic.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn stale_reject_flag_does_not_drop_new_gen_event() {
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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
        });

        let gen1_clone = gen1_tx.clone();
        let gen2_clone = gen2_tx.clone();
        let (drop_gen1_tx, mut drop_gen1_rx) = oneshot::channel::<()>();
        let (resub_signal_tx, mut resub_signal_rx) = mpsc::channel::<()>(8);

        tokio::spawn(async move {
            // Every SubscribeEvents is delayed 50 ms before the engine
            // responds, so the test window between
            //   "first resubscribe completes" and
            //   "Reloaded arm's resubscribe would complete"
            // is wide enough to deterministically land a gen2 event in
            // the WRONG receiver (the one Reloaded's resubscribe
            // replaces) — exposing any "Reloaded drops the receiver
            // from a prior Closed-branch resubscribe" bug.
            let mut gen1: Option<broadcast::Sender<DaemonEvent>> = Some(gen1_clone);
            loop {
                if gen1.is_some() {
                    tokio::select! {
                        _ = &mut drop_gen1_rx => { gen1 = None; }
                        msg = ctl_rx.recv() => {
                            match msg {
                                Some(ControlMsg::SubscribeEvents(tx)) => {
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                    if let Some(ref g1) = gen1 {
                                        let _ = tx.send(g1.subscribe());
                                    }
                                    let _ = resub_signal_tx.try_send(());
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
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            let _ = tx.send(gen2_clone.subscribe());
                            let _ = resub_signal_tx.try_send(());
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

        // Wait for the initial subscription to land.
        let _ = resub_signal_rx.recv().await;

        // Step 1: normal validation Reject — gen1 stays alive.  On the
        // OLD sticky-flag code this would set `resubscribe_on_close =
        // true`; on the new design it does nothing to the receiver.
        let _ = reload_tx.send(ReloadOutcome::Rejected("validation".into()));

        // Drain the config_reload_rejected frame emitted by the Rejected
        // arm before continuing — it arrives before the Closed/Reloaded
        // sequence below.
        let reject_drain = recv_json(&mut ws).await;
        assert_eq!(reject_drain["event"], "config_reload_rejected");
        assert_eq!(reject_drain["detail"], "validation");

        // Step 2: daemon tears down gen1 BEFORE publishing Reloaded.
        let _ = drop_gen1_tx.send(());
        drop(gen1_tx);
        let _ = reload_tx.send(ReloadOutcome::Reloaded);

        // Step 3: wait for the FIRST post-initial resubscribe to
        // complete.  On the new design this is the only resubscribe
        // (events stays = gen2 receiver).  On the OLD design a
        // SECOND resubscribe from the Reloaded arm is still queued
        // and will drop the receiver we just installed.
        let _ = resub_signal_rx.recv().await;

        // Sleep long enough for the handler to re-enter its select!,
        // pick up Reloaded, and (on the OLD design) enter the second
        // resubscribe's 50 ms await.  Without this sleep we race
        // against the handler waking and may forward the event on the
        // first receiver before the OLD design drops it.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Step 4: a gen2 event.  On the new design it lands on the
        // (only) gen2 receiver the handler holds; on the OLD design it
        // lands in the buffer of the receiver Reloaded's resubscribe
        // is about to replace, and is DROPPED.
        let _ = gen2_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("e-reload".to_string()),
            present: true,
            cause: dormant_core::types::SensorId("e".to_string()),
        });

        // Frame order is timing-dependent (which branch of the
        // select! wins: the ev_rx that holds the gen2 event, or the
        // queued Reloaded).  Collect both, regardless of order.
        let mut got_config_reloaded = false;
        let mut got_e_reload = false;
        for _ in 0..2 {
            let f = recv_json(&mut ws).await;
            match f["event"].as_str() {
                Some("config_reloaded") => got_config_reloaded = true,
                Some("zone_changed") if f["zone"] == "e-reload" => got_e_reload = true,
                other => panic!("unexpected frame: {other:?}, full={f}"),
            }
        }
        assert!(
            got_config_reloaded,
            "missing config_reloaded frame after Reloaded"
        );
        assert!(
            got_e_reload,
            "missing e-reload zone_changed frame — Reloaded's second resubscribe dropped the buffered gen2 event"
        );

        let _ = ws.close(None).await;
    }

    /// Flowing arm (events = Some): a normal `Rejected` must emit a
    /// `config_reload_rejected` WS frame with the detail.
    #[tokio::test]
    async fn rejected_emits_config_reload_rejected_frame_flowing() {
        let harness = harness().await;

        let url = harness.ws_url();
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Baseline event to confirm the WS is live.
        let _ = harness.event_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("flowing-pre".to_string()),
            state: SensorState::Present,
        });
        let f1 = recv_json(&mut ws).await;
        assert_eq!(f1["event"], "sensor_changed");
        assert_eq!(f1["sensor"], "flowing-pre");

        // Normal reject — flowing arm should emit the reject frame.
        let _ = harness
            .reload_tx
            .send(ReloadOutcome::Rejected("bad sensor config".into()));

        let reject_frame = recv_json(&mut ws).await;
        assert_eq!(reject_frame["event"], "config_reload_rejected");
        assert_eq!(reject_frame["detail"], "bad sensor config");

        // Events channel is still valid — subsequent events arrive.
        let _ = harness.event_tx.send(DaemonEvent::SensorChanged {
            sensor: dormant_core::types::SensorId("flowing-post".to_string()),
            state: SensorState::Absent,
        });
        let f2 = recv_json(&mut ws).await;
        assert_eq!(f2["event"], "sensor_changed");
        assert_eq!(f2["sensor"], "flowing-post");

        let _ = ws.close(None).await;
        harness.cancel.cancel();
    }

    /// Recovery arm (events = None): a `Rejected` when the events channel
    /// is dead must still emit the `config_reload_rejected` frame before
    /// resubscribing.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn rejected_recovery_emits_config_reload_rejected_frame() {
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
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
        });

        let gen1_clone = gen1_tx.clone();
        let gen2_clone = gen2_tx.clone();
        let (drop_gen1_tx, mut drop_gen1_rx) = oneshot::channel::<()>();
        let (resub_signal_tx, mut resub_signal_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            let mut gen1: Option<broadcast::Sender<DaemonEvent>> = Some(gen1_clone);
            let mut first_post_close = true;
            loop {
                if gen1.is_some() {
                    tokio::select! {
                        _ = &mut drop_gen1_rx => { gen1 = None; }
                        msg = ctl_rx.recv() => {
                            if let Some(ControlMsg::SubscribeEvents(tx)) = msg
                                && let Some(ref g1) = gen1
                            {
                                let _ = tx.send(g1.subscribe());
                            }
                        }
                    }
                } else if first_post_close {
                    // First post-close SubscribeEvents: drop the oneshot
                    // so the handler stays in events=None.  This forces
                    // the Rejected outcome to arrive through the recovery
                    // arm (the flowing arm is unreachable with events
                    // still None).
                    first_post_close = false;
                    let msg = ctl_rx.recv().await;
                    if let Some(ControlMsg::SubscribeEvents(tx)) = msg {
                        drop(tx);
                    }
                } else {
                    let msg = ctl_rx.recv().await;
                    if let Some(ControlMsg::SubscribeEvents(tx)) = msg {
                        let _ = tx.send(gen2_clone.subscribe());
                        let _ = resub_signal_tx.send(()).await;
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
            sensor: dormant_core::types::SensorId("recovery-pre".to_string()),
            state: SensorState::Present,
        });
        let f1 = recv_json(&mut ws).await;
        assert_eq!(f1["event"], "sensor_changed");
        assert_eq!(f1["sensor"], "recovery-pre");

        // Tear down gen1.  The handler calls SubscribeEvents; the engine
        // drops the oneshot so the handler sits in events=None.
        let _ = drop_gen1_tx.send(());
        drop(gen1_tx);
        // One turn for the handler to drain Closed, attempt the (failed)
        // resubscribe, and re-enter the events=None select!.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send Rejected NOW, while the handler is genuinely in
        // events=None.  The frame can only come from the recovery arm.
        let _ = reload_tx.send(ReloadOutcome::Rejected("rebuild rejected".into()));

        // The handler must emit config_reload_rejected BEFORE resubscribing.
        let reject_frame = recv_json(&mut ws).await;
        assert_eq!(reject_frame["event"], "config_reload_rejected");
        assert_eq!(reject_frame["detail"], "rebuild rejected");

        // Wait until resubscribed to gen2.
        let _ = resub_signal_rx.recv().await;

        // gen2 events arrive after recovery.
        let _ = gen2_tx.send(DaemonEvent::ZoneChanged {
            zone: dormant_core::types::ZoneId("recovery-post".to_string()),
            present: true,
            cause: dormant_core::types::SensorId("gen2".to_string()),
        });

        let f2 = recv_json(&mut ws).await;
        assert_eq!(f2["event"], "zone_changed");
        assert_eq!(f2["zone"], "recovery-post");

        let _ = ws.close(None).await;
    }
}
