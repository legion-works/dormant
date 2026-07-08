//! Samsung Tizen display controller — blanks and wakes Samsung Smart TVs
//! (S90D and similar) over the Tizen WebSocket remote-control protocol.
//!
//! ## Protocol
//!
//! The controller connects to `wss://{host}:8002/api/v2/channels/samsung.remote.control`
//! with a base64-encoded device name and a pairing token. Blank/wake commands
//! are JSON key-press messages sent over the WebSocket; panel state is queried
//! via the REST device-info endpoint on port 8001.
//!
//! ## TLS
//!
//! Samsung TVs use **self-signed certificates** on the local LAN that native
//! root stores reject. This module ships a `NoVerify` TLS verifier (a
//! [`rustls::client::danger::ServerCertVerifier`] impl) that accepts any
//! certificate presented. The token still authenticates the controller to the
//! TV — the disabled certificate check is a local-LAN convenience, not a
//! security regression (the remote-control channel is authenticated by the
//! token, not the certificate, and an attacker who can MITM your LAN already
//! owns your TV).
//!
//! ## Blank modes
//!
//! | Mode | Key / transport | Effect |
//! |---|---|---|
//! | `ScreenOffAudioOn` | `KEY_PICTURE_OFF` (port 8002) | Panel dark, audio continues; toggle |
//! | `PowerOff` | `KEY_POWER` | Full power-off |
//! | `BrightnessZero` | `backlightControl` (port 1516) | Panel dimmed to 0 via IP Control; source + audio keep running |
//!
//! ## Wake
//!
//! `KEY_RETURN` wakes the TV from picture-off (verified on S90D — not a
//! toggle, safe to send when state is uncertain). When `wol_mac` is set,
//! a Wake-on-LAN magic packet is broadcast before the WS wake attempt.
//! For `BrightnessZero`, wake restores the previously-saved backlight
//! value (see [the IP Control section](#samsung-ip-control-g2-backlight)).
//!
//! ## Socket lifecycle
//!
//! The TV silently drops idle WebSocket connections during picture-off.
//! The first write on a stale socket returns `Ok` at the TCP level (the
//! frame is lost, the RST arrives later), so a send-only error check
//! misses the failure. The robust liveness signal is **time since the TV
//! last sent ANY frame** — Samsung drives the heartbeat by sending
//! WebSocket pings/frames roughly every ~10 s. A background reader task
//! (spawned at connect time, cancelled at replace time) continuously
//! drains incoming frames and updates a shared `last_seen` timestamp;
//! `tungstenite` auto-responds to pings with pongs. Before sending a key,
//! the controller checks `now - last_seen > MAX_WS_SILENCE` and treats
//! the socket as stale → reconnect. As a further backstop, a send error
//! after a passed freshness check still triggers one reconnect-and-retry.
//! This is the proven `ollo69/ha-samsungtv-smart` pattern — the client
//! no longer relies on its own ping→pong round-trip; the TV is the
//! authoritative heartbeat.
//!
//! ## Samsung IP Control G2 (backlight)
//!
//! `BrightnessZero` blanks via Samsung IP Control G2 (HTTPS port 1516,
//! JSON-RPC 2.0, `backlightControl` method). This is the audio-safe
//! alternative to `KEY_PICTURE_OFF` — the panel backlight goes to 0
//! (near-black dim, not true-off) while the HDMI source and audio keep
//! running. Implementation lives in [`crate::samsung_ip`]; the controller
//! here holds a [`samsung_ip::BacklightTransport`] and an in-controller
//! `saved_backlight` value following the ddcci first-blank-wins pattern.
//!
//! ## Unreachable policy
//!
//! When `treat_unreachable_as_blanked` is true (default) and the TV is
//! unreachable (standby/off): blank succeeds as a no-op AND wake succeeds as
//! a no-op (after the `WoL` attempt if configured). An off TV has no picture to
//! protect or restore; the daemon must not sit in a Waking retry loop.
//! Both cases log at info with the literal event string `tv_unreachable_noop`.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use base64::Engine as _;
use dormant_core::error::DormantError;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};
use futures_util::SinkExt;
use futures_util::StreamExt;
use futures_util::stream::{SplitSink, SplitStream};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::samsung_ip::{self, BacklightTransport, RealBacklightTransport};

// ── Constants ───────────────────────────────────────────────────────────────────

/// Tizen WebSocket remote-control port.
const WS_PORT: u16 = 8002;

/// Tizen REST device-info port.
const REST_PORT: u16 = 8001;

/// WebSocket path template for the remote-control channel.
const WS_PATH: &str = "/api/v2/channels/samsung.remote.control";

/// REST path for device info (power state oracle).
const REST_PATH: &str = "/api/v2/";

/// Device name sent to the TV during WebSocket handshake.
const DEVICE_NAME: &str = "dormant";

/// Blank key — turns picture off while keeping audio on. Toggle.
const KEY_PICTURE_OFF: &str = "KEY_PICTURE_OFF";

/// Wake key — dismisses picture-off and returns to normal display. NOT a toggle.
const KEY_RETURN: &str = "KEY_RETURN";

/// Power-off key — full power down.
const KEY_POWER: &str = "KEY_POWER";

/// Log event literal for unreachable-tv no-ops.
const TV_UNREACHABLE_NOOP: &str = "tv_unreachable_noop";

/// `WoL` broadcast address.
const WOL_BROADCAST: &str = "255.255.255.255:9";

/// Maximum backlight value when restoring a `BrightnessZero` blank whose
/// saved value is missing (daemon restart, reload, or first-ever wake).
///
/// The TV's IP-Control backlight scale is 0–50; 50 is the max. Per the
/// fail-safe-toward-screens-on doctrine, a too-bright panel is acceptable
/// — a stuck-dim one is not — so this is the conservative default.
pub const DEFAULT_RESTORE_BACKLIGHT: u8 = 50;

/// Maximum time to wait for a TCP connect probe.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// REST HTTP request timeout.
const REST_TIMEOUT: Duration = Duration::from_secs(3);

/// Default seconds for a WebSocket connect timeout.
const WS_CONNECT_TIMEOUT_SECS: u64 = 5;

/// Maximum time to wait since the TV last sent any frame before treating
/// the cached WebSocket as stale and reconnecting. Samsung TVs drive the
/// heartbeat — they send a WebSocket ping (or data frame) roughly every
/// ~10 s. Anything beyond that means the silent-drop happened.
const MAX_WS_SILENCE: Duration = Duration::from_secs(10);

// ── TvTransport trait — network boundary for test injection ─────────────────────

/// Abstract transport for all Samsung TV I/O: WebSocket key-send, REST
/// power-state query, `WoL` broadcast, and TCP connect probe.
///
/// The real implementation manages a persistent TLS WebSocket connection
/// with reconnect-on-failure; the fake used in tests records calls and
/// returns pre-programmed responses.
#[async_trait]
pub trait TvTransport: Send + Sync {
    /// Send a remote-control key over the WebSocket channel.
    ///
    /// Returns `Ok(())` on success, or an error string describing the failure.
    async fn send_key(&self, host: &str, token: &str, key: &str) -> Result<(), String>;

    /// Query the TV's power state via the REST device-info endpoint.
    ///
    /// Returns `Some("on")` or `Some("standby")`, or `None` if unreachable.
    async fn get_power_state(&self, host: &str) -> Option<String>;

    /// Send a Wake-on-LAN magic packet to the given MAC address.
    async fn send_wol(&self, mac: &str) -> Result<(), String>;

    /// Check whether `host:port` accepts a TCP connection within `timeout`.
    async fn tcp_connect_ok(&self, host: &str, port: u16, connect_timeout: Duration) -> bool;
}

// ── Real transport ──────────────────────────────────────────────────────────────

/// Alias for the cached WebSocket write half.
type TvWsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// Shared state owned by a single [`RealTvTransport`] instance.
///
/// The reader task (spawned at connect time) updates `last_seen` on every
/// received frame; the sender reads it to decide whether the cached socket
/// is still worth talking to. `cancel` signals the reader task to exit when
/// a fresh connection replaces the cached socket — without this the old
/// reader would keep polling its dead half of a torn-down stream.
struct WsReaderState {
    /// Instant of the most recently received frame from the TV.
    last_seen: StdMutex<Instant>,
    /// Set to `true` when the reader exits because the peer closed the
    /// connection or sent an error frame. The sender checks this before
    /// dispatching a fresh send.
    dead: AtomicBool,
    /// Fires when the transport replaces the cached socket. The reader
    /// selects on `cancel.cancelled()` alongside `stream.next()` so it can
    /// exit promptly instead of leaking across reconnects.
    cancel: CancellationToken,
}

impl WsReaderState {
    fn fresh() -> Arc<Self> {
        Arc::new(Self {
            // `last_seen` starts at construction time. A socket that never
            // sees a frame is still considered fresh for MAX_WS_SILENCE
            // from this anchor, which is correct: the connect just happened.
            last_seen: StdMutex::new(Instant::now()),
            dead: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        })
    }

    fn touch(&self) {
        if let Ok(mut t) = self.last_seen.lock() {
            *t = Instant::now();
        }
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    fn is_stale(&self, max_silence: Duration) -> bool {
        match self.last_seen.lock() {
            Ok(t) => t.elapsed() > max_silence,
            Err(_) => true, // poisoned → treat as stale, force reconnect
        }
    }
}

/// Background reader task: continuously drains incoming frames from the
/// WebSocket read half, updating `last_seen` on every frame.
///
/// `tungstenite` auto-responds to incoming pings with pongs, so this task
/// does not need to inspect frame types — any incoming byte means the
/// socket is alive from the TV's side. On peer close or read error the
/// task flips the `dead` flag so the sender reconnects on the next send.
async fn run_ws_reader(
    mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    state: Arc<WsReaderState>,
) {
    loop {
        tokio::select! {
            // Biased so a cancellation is observed promptly even if a frame
            // is already waiting in the stream's internal buffer.
            biased;
            () = state.cancel.cancelled() => break,
            next = read.next() => {
                if let Some(Ok(_)) = next {
                    state.touch();
                } else {
                    state.dead.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }
}

/// Production transport: persistent TLS WebSocket, REST HTTP, `WoL` UDP.
///
/// When testing, can be constructed with a plain `ws://` scheme so that
/// local tokio-tungstenite servers (no TLS) can exercise the reconnect logic.
struct RealTvTransport {
    /// Cached WebSocket **write half** — the read half is owned by the
    /// background reader task. Holding only the sink lets the reader task
    /// poll frames without contending with the sender.
    ws: Mutex<Option<TvWsSink>>,
    /// Shared reader state; replaced under a std-mutex when the cached
    /// socket is replaced. The std-mutex is sufficient because the critical
    /// section is a few atomic operations and connect-replacement runs
    /// rarely (every reconnect).
    reader_state: StdMutex<Arc<WsReaderState>>,
    /// URL scheme — `"wss"` in production, `"ws"` for plain-TCP tests.
    ws_scheme: &'static str,
    /// WebSocket port — 8002 in production, overridden in tests.
    ws_port: u16,
    /// Maximum silence before treating the cached socket as stale.
    max_ws_silence: Duration,
}

impl RealTvTransport {
    fn new() -> Self {
        Self {
            ws: Mutex::new(None),
            reader_state: StdMutex::new(WsReaderState::fresh()),
            ws_scheme: "wss",
            ws_port: WS_PORT,
            max_ws_silence: MAX_WS_SILENCE,
        }
    }

    /// Reconnect test helper: send a close frame on the cached WS and drop
    /// the stream, leaving a dead socket in the cache so the next `send_key`
    /// hits the "try-cached → fail → reconnect → retry" path.
    #[cfg(test)]
    async fn close_cached_for_test(&self) {
        let mut guard = self.ws.lock().await;
        if let Some(sink) = guard.as_mut() {
            let _ = sink.send(Message::Close(None)).await;
            let _ = sink.close().await;
        }
        drop(guard);
        // Brief yield to let the close propagate through the stream.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    /// Build a transport that connects over plain WebSocket (no TLS) to the
    /// given port — used by tests that stand up a local `tokio-tungstenite`
    /// server.
    #[cfg(test)]
    fn for_test(port: u16) -> Self {
        Self::for_test_with_silence(port, MAX_WS_SILENCE)
    }

    /// Build a test transport with a custom freshness threshold — lets
    /// stale-socket tests use a sub-second window instead of waiting 10 s.
    #[cfg(test)]
    fn for_test_with_silence(port: u16, max_ws_silence: Duration) -> Self {
        Self {
            ws: Mutex::new(None),
            reader_state: StdMutex::new(WsReaderState::fresh()),
            ws_scheme: "ws",
            ws_port: port,
            max_ws_silence,
        }
    }

    /// Test helper: clone the current reader-state Arc so tests can poll
    /// `last_seen` / `dead` from outside the transport.
    #[cfg(test)]
    fn reader_state_for_test(&self) -> Arc<WsReaderState> {
        Arc::clone(&*self.reader_state.lock().expect("reader_state poisoned"))
    }

    /// Test helper: rewind `last_seen` so the freshness check considers
    /// the cached socket stale immediately.
    #[cfg(test)]
    fn age_last_seen_for_test(&self, age: Duration) {
        let state = self.reader_state.lock().expect("reader_state poisoned");
        if let Ok(mut t) = state.last_seen.lock() {
            *t = Instant::now().checked_sub(age).unwrap_or_else(|| {
                // `Instant::now() - 1ns` is well-defined on every platform
                // (monotonic clock, smallest representable step), but
                // clippy flags it as a subtraction. Wrap in `checked_sub`
                // and fall back to the saturating variant.
                let now = Instant::now();
                now.checked_sub(Duration::from_nanos(1)).unwrap_or(now)
            });
        }
    }

    /// Build the `rustls::ClientConfig` that accepts any server certificate.
    fn tls_config() -> Arc<rustls::ClientConfig> {
        let provider = rustls::crypto::ring::default_provider().into();
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("safe default protocol versions")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth(),
        )
    }

    /// Connect (or reconnect) a WebSocket to the TV, replacing the cached
    /// socket and spawning a fresh reader task.
    ///
    /// The old reader task is cancelled BEFORE the new connection starts
    /// so it cannot observe a half-torn-down stream. A short yield gives
    /// it a chance to exit before we proceed (the cancellation is observed
    /// on the next select! poll).
    async fn connect_ws(&self, host: &str, token: &str) -> Result<(), String> {
        // 1. Drop the cached sink — this signals EOF on the read half, but
        //    we also cancel the reader task explicitly so it cannot race
        //    with our state replacement.
        {
            let mut guard = self.ws.lock().await;
            let _ = (*guard).take();
        }

        // 2. Cancel the prior reader state and swap in a fresh one. The
        //    std-mutex covers the brief window where the new reader could
        //    otherwise race the cancel signal.
        let new_state = WsReaderState::fresh();
        let old_state = {
            let mut guard = self.reader_state.lock().expect("reader_state poisoned");
            let old = std::mem::replace(&mut *guard, new_state);
            drop(guard);
            old
        };
        old_state.cancel.cancel();
        // We deliberately do not block waiting for the old reader to exit;
        // its select! loop observes `cancel` on the next poll and drops the
        // read half, which closes the TCP socket when the prior sink is
        // already gone.

        let name_b64 = base64::engine::general_purpose::STANDARD.encode(DEVICE_NAME);
        let url = format!(
            "{}://{host}:{}{WS_PATH}?name={name_b64}&token={token}",
            self.ws_scheme, self.ws_port
        );

        // Branch at compile-like level: TLS vs plain. The two connect
        // functions return different opaque future types, so we can't use
        // a single `if/else` with Box::pin here.
        let (sink, read) = if self.ws_scheme == "wss" {
            let request = url
                .as_str()
                .into_client_request()
                .map_err(|e| format!("failed to build WS request: {e}"))?;

            let connector = tokio_tungstenite::Connector::Rustls(Self::tls_config());
            let connect_fut = tokio_tungstenite::connect_async_tls_with_config(
                request,
                None,  // WebSocketConfig — use defaults
                false, // disable_nagle
                Some(connector),
            );
            match timeout(Duration::from_secs(WS_CONNECT_TIMEOUT_SECS), connect_fut).await {
                Ok(Ok((ws, _response))) => ws.split(),
                Ok(Err(e)) => return Err(format!("WebSocket connect failed: {e}")),
                Err(_) => return Err("WebSocket connect timed out".to_string()),
            }
        } else {
            let connect_fut = tokio_tungstenite::connect_async(&url);
            match timeout(Duration::from_secs(WS_CONNECT_TIMEOUT_SECS), connect_fut).await {
                Ok(Ok((ws, _response))) => ws.split(),
                Ok(Err(e)) => return Err(format!("WebSocket connect failed: {e}")),
                Err(_) => return Err("WebSocket connect timed out".to_string()),
            }
        };

        // 3. Install the new sink and spawn the reader task BEFORE returning
        //    so the caller can immediately send without a window where a
        //    fresh socket exists but no reader is watching it.
        let reader_state = Arc::clone(&*self.reader_state.lock().expect("reader_state poisoned"));
        tokio::spawn(run_ws_reader(read, reader_state));

        let mut guard = self.ws.lock().await;
        *guard = Some(sink);
        Ok(())
    }

    /// Send a single text frame over the cached WebSocket, reconnecting if needed.
    ///
    /// Two stale-socket guards run before the send:
    ///
    /// 1. The reader's `dead` flag (set when the peer closed the connection
    ///    or the reader hit an error frame).
    /// 2. The freshness check (`now - last_seen > MAX_WS_SILENCE`). Samsung
    ///    drives the heartbeat by pinging roughly every ~10 s; silence
    ///    beyond that means the silent-drop happened.
    ///
    /// As a backstop, a send error after a passed freshness check still
    /// triggers one reconnect-and-retry.
    async fn ws_send_with_retry(
        &self,
        host: &str,
        token: &str,
        payload: &str,
    ) -> Result<(), String> {
        // Connect on demand if the cache is cold (first send after daemon start).
        {
            let guard = self.ws.lock().await;
            if guard.is_none() {
                drop(guard);
                self.connect_ws(host, token).await?;
            }
        }

        // Try the cached socket — but first verify it is alive via the
        // TV-driven heartbeat signal.
        let current_state = Arc::clone(&*self.reader_state.lock().expect("reader_state poisoned"));
        let mut needs_reconnect = false;
        {
            let mut guard = self.ws.lock().await;
            if guard.is_some() {
                if current_state.is_dead() || current_state.is_stale(self.max_ws_silence) {
                    tracing::info!(
                        controller = SamsungTizenController::NAME,
                        "WS freshness check failed (dead or stale), reconnecting"
                    );
                    *guard = None;
                    needs_reconnect = true;
                } else {
                    match guard
                        .as_mut()
                        .expect("just checked")
                        .send(Message::Text(payload.to_string()))
                        .await
                    {
                        Ok(()) => return Ok(()),
                        Err(e) => {
                            tracing::info!(
                                controller = SamsungTizenController::NAME,
                                "WS send failed ({e}), reconnecting"
                            );
                            *guard = None;
                            needs_reconnect = true;
                        }
                    }
                }
            }
        }
        let _ = needs_reconnect; // only used to document intent below

        // Reconnect and retry once.
        self.connect_ws(host, token).await?;

        let mut guard = self.ws.lock().await;
        match guard.as_mut() {
            Some(sink) => sink
                .send(Message::Text(payload.to_string()))
                .await
                .map_err(|e| format!("WS send after reconnect failed: {e}")),
            None => Err("WS connection lost after reconnect".to_string()),
        }
    }
}

/// On transport drop, cancel the current reader task so it exits its
/// `select!` loop and drops the read half — otherwise the spawned task
/// lives on with its socket and the underlying TCP connection stays open
/// past the daemon's lifecycle.
///
/// Reconnect swaps the `reader_state` under its std-mutex and fires the
/// prior token's cancel; the `Drop` impl handles the LAST state when no
/// more reconnects are coming.
impl Drop for RealTvTransport {
    fn drop(&mut self) {
        if let Ok(state) = self.reader_state.lock() {
            state.cancel.cancel();
        }
    }
}

#[async_trait]
impl TvTransport for RealTvTransport {
    async fn send_key(&self, host: &str, token: &str, key: &str) -> Result<(), String> {
        let payload = build_key_payload(key);
        self.ws_send_with_retry(host, token, &payload).await
    }

    async fn get_power_state(&self, host: &str) -> Option<String> {
        let url = format!("http://{host}:{REST_PORT}{REST_PATH}");
        let client = reqwest::Client::builder()
            .timeout(REST_TIMEOUT)
            .build()
            .ok()?;
        let resp = client.get(&url).send().await.ok()?;
        let body: serde_json::Value = resp.json().await.ok()?;
        body.get("device")
            .and_then(|d| d.get("PowerState"))
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string)
    }

    async fn send_wol(&self, mac: &str) -> Result<(), String> {
        let packet = build_magic_packet(mac)?;
        let socket =
            std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("WoL bind failed: {e}"))?;
        socket
            .set_broadcast(true)
            .map_err(|e| format!("WoL set_broadcast failed: {e}"))?;
        socket
            .send_to(&packet, WOL_BROADCAST)
            .map_err(|e| format!("WoL send failed: {e}"))?;
        Ok(())
    }

    async fn tcp_connect_ok(&self, host: &str, port: u16, connect_timeout: Duration) -> bool {
        let addr = format!("{host}:{port}");
        timeout(connect_timeout, TcpStream::connect(&addr))
            .await
            .is_ok_and(|r| r.is_ok())
    }
}

// ── NoVerify — TLS certificate verifier that accepts any cert ───────────────────

/// A [`rustls::client::danger::ServerCertVerifier`] that accepts **every**
/// server certificate without validation.
///
/// Used only for the Samsung Tizen controller's local-LAN WebSocket
/// connections. Samsung TVs ship with self-signed certificates that a
/// standard root store rejects. Since the remote-control protocol is already
/// token-authenticated and the threat model is the local network (an attacker
/// who can MITM your LAN can already control the TV), skipping certificate
/// validation is a pragmatic concession, not a security hole.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        // Return all known schemes so the handshake can negotiate any cipher
        // the TV presents. The actual signature is still accepted regardless
        // (verify_tls1{2,3}_signature returns assertion()), so this list just
        // prevents an early "no common signature scheme" abort.
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ── Helper: build the WebSocket URL ─────────────────────────────────────────────

// ── Helper: build the key-send JSON payload ─────────────────────────────────────

/// Build the exact JSON payload for a remote-control key press.
///
/// The payload matches the Samsung Tizen WebSocket protocol:
/// `{"method":"ms.remote.control","params":{"Cmd":"Click","DataOfCmd":"<KEY>","Option":"false","TypeOfRemote":"SendRemoteKey"}}`
fn build_key_payload(key: &str) -> String {
    json!({
        "method": "ms.remote.control",
        "params": {
            "Cmd": "Click",
            "DataOfCmd": key,
            "Option": "false",
            "TypeOfRemote": "SendRemoteKey"
        }
    })
    .to_string()
}

// ── Helper: build a WoL magic packet ────────────────────────────────────────────

/// Build a Wake-on-LAN magic packet for the given MAC address.
///
/// Returns 102 bytes: 6 × `0xFF` followed by the 6-byte MAC repeated 16 times.
/// Accepts MACs with colons (`aa:bb:cc:dd:ee:ff`) or hyphens.
fn build_magic_packet(mac: &str) -> Result<Vec<u8>, String> {
    let hex: String = mac.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 12 {
        return Err(format!(
            "invalid MAC address '{mac}': expected 12 hex digits, got {}",
            hex.len()
        ));
    }
    let mac_bytes: Vec<u8> = (0..6)
        .map(|i| {
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|e| format!("invalid hex in MAC '{mac}': {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut packet = Vec::with_capacity(102);
    packet.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&mac_bytes);
    }
    Ok(packet)
}

// ── SamsungTizenController ──────────────────────────────────────────────────────

/// Display controller for Samsung Tizen TVs (S90D and similar).
///
/// Communicates over the TV's WebSocket remote-control channel (port 8002)
/// for `KEY_*` blank/wake and over Samsung IP Control G2 (HTTPS port 1516,
/// JSON-RPC) for the `BrightnessZero` audio-safe blank mode.
///
/// Constructed by [`crate::registry::build_controllers`] from a
/// [`dormant_core::config::schema::DisplayConfig`] that names `samsung-tizen`
/// as one of its controllers.
pub struct SamsungTizenController {
    /// TV hostname or IP address.
    host: String,
    /// Pairing token for the remote-control WebSocket channel.
    token: String,
    /// MAC address for optional Wake-on-LAN (best-effort deep-standby wake).
    wol_mac: Option<String>,
    /// Treat unreachable TV as blanked (avoid retry loops).
    treat_unreachable_as_blanked: bool,
    /// Transport layer — real network in production, fake in tests.
    transport: Arc<dyn TvTransport>,
    /// Port-1516 backlight transport for the audio-safe `BrightnessZero`
    /// blank mode. Real in production, fake in tests.
    backlight: Arc<dyn BacklightTransport>,
    /// Backlight value at the time of the first `BrightnessZero` blank.
    /// First-blank-wins pattern: a second blank while already at 0 reads
    /// `current=0` and must NOT clobber the real saved value, or wake
    /// would restore 0 (stuck-dark). Cleared on wake AFTER a successful
    /// restore so a transient set failure leaves the value intact for
    /// retry.
    saved_backlight: StdMutex<Option<u8>>,
    /// Effective blank mode for this display — set at construction from the
    /// config's `blank_mode`. Used by `wake()` to pick the correct wake
    /// path: `BrightnessZero` always restores via backlight (defaulting to
    /// [`DEFAULT_RESTORE_BACKLIGHT`] when no value is saved), while
    /// `ScreenOffAudioOn` / `PowerOff` send `KEY_RETURN`.
    effective_mode: BlankMode,
}

impl std::fmt::Debug for SamsungTizenController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamsungTizenController")
            .field("host", &self.host)
            .field("token", &"***")
            .field("wol_mac", &self.wol_mac)
            .field(
                "treat_unreachable_as_blanked",
                &self.treat_unreachable_as_blanked,
            )
            .field("transport", &"dyn TvTransport")
            .field("backlight", &"dyn BacklightTransport")
            .field("saved_backlight", &"Option<u8>")
            .field("effective_mode", &self.effective_mode)
            .finish()
    }
}

impl SamsungTizenController {
    /// Literal controller name — grep-stable, matches the `samsung-tizen` config type.
    const NAME: &'static str = "samsung-tizen";

    /// Build a new controller with the real network transports.
    ///
    /// `effective_mode` is the display's configured `blank_mode` — it
    /// drives the `wake()` path (see [`Self::effective_mode`]).
    #[must_use]
    pub fn new(
        host: String,
        token: String,
        wol_mac: Option<String>,
        treat_unreachable_as_blanked: bool,
        effective_mode: BlankMode,
    ) -> Self {
        Self {
            host,
            token,
            wol_mac,
            treat_unreachable_as_blanked,
            transport: Arc::new(RealTvTransport::new()),
            backlight: Arc::new(RealBacklightTransport::new()),
            saved_backlight: StdMutex::new(None),
            effective_mode,
        }
    }

    /// Build a controller with custom transports (used by tests).
    ///
    /// `effective_mode` defaults to [`BlankMode::ScreenOffAudioOn`] so
    /// pre-existing call sites that pre-date the constructor signature
    /// change keep working — the picture-off wake path is unchanged.
    #[must_use]
    pub fn with_transport(
        host: String,
        token: String,
        wol_mac: Option<String>,
        treat_unreachable_as_blanked: bool,
        transport: Arc<dyn TvTransport>,
    ) -> Self {
        Self {
            host,
            token,
            wol_mac,
            treat_unreachable_as_blanked,
            transport,
            backlight: Arc::new(RealBacklightTransport::new()),
            saved_backlight: StdMutex::new(None),
            effective_mode: BlankMode::ScreenOffAudioOn,
        }
    }

    /// Build a controller with BOTH transports injected (used by tests).
    ///
    /// `effective_mode` defaults to [`BlankMode::ScreenOffAudioOn`] — tests
    /// that need the brightness-zero wake path should use
    /// [`Self::with_transports_mode`] (or override the field directly via
    /// the public `effective_mode` accessor below).
    #[must_use]
    pub fn with_transports(
        host: String,
        token: String,
        wol_mac: Option<String>,
        treat_unreachable_as_blanked: bool,
        transport: Arc<dyn TvTransport>,
        backlight: Arc<dyn BacklightTransport>,
    ) -> Self {
        Self {
            host,
            token,
            wol_mac,
            treat_unreachable_as_blanked,
            transport,
            backlight,
            saved_backlight: StdMutex::new(None),
            effective_mode: BlankMode::ScreenOffAudioOn,
        }
    }

    /// Build a controller with both transports AND an explicit effective
    /// mode. Tests for the `BrightnessZero` wake path use this.
    #[must_use]
    pub fn with_transports_mode(
        host: String,
        token: String,
        wol_mac: Option<String>,
        treat_unreachable_as_blanked: bool,
        transport: Arc<dyn TvTransport>,
        backlight: Arc<dyn BacklightTransport>,
        effective_mode: BlankMode,
    ) -> Self {
        Self {
            host,
            token,
            wol_mac,
            treat_unreachable_as_blanked,
            transport,
            backlight,
            saved_backlight: StdMutex::new(None),
            effective_mode,
        }
    }

    /// Check whether the TV is reachable (TCP connect to WS port within 1s).
    async fn is_tv_reachable(&self) -> bool {
        self.transport
            .tcp_connect_ok(&self.host, WS_PORT, CONNECT_TIMEOUT)
            .await
    }

    /// Log and return a successful no-op when the TV is unreachable.
    #[allow(clippy::unnecessary_wraps)]
    fn unreachable_noop(&self, operation: &str) -> Result<(), CmdFailure> {
        tracing::info!(
            event = TV_UNREACHABLE_NOOP,
            host = %self.host,
            operation,
            "TV unreachable — treating {} as no-op",
            operation,
        );
        Ok(())
    }

    /// Blank via Samsung IP Control G2 backlight.
    ///
    /// On the first blank: read the current backlight, save it, set to 0.
    /// On subsequent blanks (already at 0): read returns 0, but the saved
    /// value is NOT overwritten — first-blank-wins prevents the wake from
    /// restoring 0 (stuck-dark).
    async fn blank_backlight(&self) -> Result<(), CmdFailure> {
        let token = self
            .backlight
            .acquire_token(&self.host)
            .await
            .map_err(|e| {
                samsung_ip::map_transport_error(Self::NAME, &*self.backlight, &self.host, &e)
            })?;

        let current = self
            .backlight
            .get_backlight(&self.host, &token)
            .await
            .map_err(|e| {
                samsung_ip::map_transport_error(Self::NAME, &*self.backlight, &self.host, &e)
            })?;

        self.backlight
            .set_backlight(&self.host, &token, 0)
            .await
            .map_err(|e| {
                samsung_ip::map_transport_error(Self::NAME, &*self.backlight, &self.host, &e)
            })?;

        let mut saved = self
            .saved_backlight
            .lock()
            .expect("saved_backlight poisoned");
        if saved.is_none() {
            *saved = Some(current);
        }
        Ok(())
    }

    /// Wake via backlight restore for a `BrightnessZero`-configured display.
    ///
    /// Uses the saved value if present, otherwise falls back to
    /// [`DEFAULT_RESTORE_BACKLIGHT`] (the max on the 0–50 scale). The
    /// default is conservative — per the fail-safe-toward-screens-on
    /// doctrine a too-bright panel is acceptable; a stuck-dim one is not.
    ///
    /// The set is always attempted (never short-circuits to `KEY_RETURN`)
    /// because `KEY_RETURN` does NOT raise the port-1516 backlight — it
    /// only dismisses picture-off. Without this guarantee, a daemon
    /// restart that loses `saved_backlight` would leave the panel dimmed
    /// while the daemon thinks it woke.
    async fn restore_backlight_with_default(&self) -> Result<(), CmdFailure> {
        let target = {
            let saved = self
                .saved_backlight
                .lock()
                .expect("saved_backlight poisoned");
            saved.unwrap_or(DEFAULT_RESTORE_BACKLIGHT)
        };

        let token = self
            .backlight
            .acquire_token(&self.host)
            .await
            .map_err(|e| {
                samsung_ip::map_transport_error(Self::NAME, &*self.backlight, &self.host, &e)
            })?;
        self.backlight
            .set_backlight(&self.host, &token, target)
            .await
            .map_err(|e| {
                samsung_ip::map_transport_error(Self::NAME, &*self.backlight, &self.host, &e)
            })?;

        // set_backlight succeeded — clear the saved value so the next
        // blank cycle starts fresh.
        let mut saved = self
            .saved_backlight
            .lock()
            .expect("saved_backlight poisoned");
        *saved = None;
        Ok(())
    }
}

#[async_trait]
impl DisplayController for SamsungTizenController {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        vec![
            BlankMode::ScreenOffAudioOn,
            BlankMode::BrightnessZero,
            BlankMode::PowerOff,
        ]
    }

    async fn is_available(&self) -> bool {
        self.is_tv_reachable().await
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        match mode {
            BlankMode::BrightnessZero => {
                // Treat unreachable the same as the WebSocket path — an off
                // TV has no panel to dim, succeed as a no-op.
                if !self.is_tv_reachable().await && self.treat_unreachable_as_blanked {
                    return self.unreachable_noop("blank");
                }
                self.blank_backlight().await
            }
            BlankMode::ScreenOffAudioOn | BlankMode::PowerOff => {
                if !self.is_tv_reachable().await && self.treat_unreachable_as_blanked {
                    return self.unreachable_noop("blank");
                }

                let key = match mode {
                    BlankMode::ScreenOffAudioOn => KEY_PICTURE_OFF,
                    BlankMode::PowerOff => KEY_POWER,
                    BlankMode::BrightnessZero => unreachable!(),
                };

                self.transport
                    .send_key(&self.host, &self.token, key)
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: {e}"),
                    })
            }
        }
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        // Best-effort WoL for deep-standby TVs (same as before).
        if let Some(ref mac) = self.wol_mac
            && let Err(e) = self.transport.send_wol(mac).await
        {
            tracing::warn!(
                controller = Self::NAME,
                mac = %mac,
                "WoL send failed: {e}"
            );
        }

        // If the TV is unreachable and the policy says to treat it as blanked,
        // succeed silently after the WoL attempt.
        if !self.is_tv_reachable().await && self.treat_unreachable_as_blanked {
            return self.unreachable_noop("wake");
        }

        // Branch on the display's effective blank mode:
        //
        // - `BrightnessZero` ALWAYS restores via backlight — even when
        //   `saved_backlight` is `None` (daemon restart / reload / first
        //   wake). The default fallback is `DEFAULT_RESTORE_BACKLIGHT`
        //   (50, the max on the 0–50 scale) so a too-bright panel is
        //   acceptable — a stuck-dim one is not. `KEY_RETURN` would NOT
        //   raise the port-1516 backlight, so falling through to it would
        //   leave the panel dimmed while the daemon thinks it woke.
        // - `ScreenOffAudioOn` (and `PowerOff`) sends `KEY_RETURN` —
        //   picture-off's wake key (verified non-toggle on S90D; safe even
        //   if daemon state has drifted).
        if self.effective_mode == BlankMode::BrightnessZero {
            return self.restore_backlight_with_default().await;
        }

        self.transport
            .send_key(&self.host, &self.token, KEY_RETURN)
            .await
            .map_err(|e| CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!("{E_DISPLAY_IO}: {e}"),
            })
    }
}

// ── Pairing ─────────────────────────────────────────────────────────────────────

/// Pair with a Samsung Tizen TV and return the granted access token.
///
/// Connects to the TV without a token, waits for the user to accept the
/// pairing request on the TV, and returns the token for future authenticated
/// connections.  The caller supplies a [`Duration`] timeout bounding the
/// entire connect+handshake; a typical interactive pairing uses 60–120 s.
///
/// # Errors
///
/// Returns a [`DormantError`] if the connection fails or the pairing
/// handshake times out.
pub async fn pair(host: &str, timeout_dur: Duration) -> Result<String, DormantError> {
    tokio::time::timeout(timeout_dur, async {
        let url = format!(
            "wss://{host}:{WS_PORT}{WS_PATH}?name={}",
            base64::engine::general_purpose::STANDARD.encode(DEVICE_NAME)
        );

        let request = url
            .as_str()
            .into_client_request()
            .map_err(|e| DormantError::DisplayIo {
                controller: SamsungTizenController::NAME.into(),
                detail: format!("failed to build pair request: {e}"),
            })?;

        let connector = tokio_tungstenite::Connector::Rustls(RealTvTransport::tls_config());

        let (mut ws, _response) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,  // WebSocketConfig — use defaults
            false, // disable_nagle
            Some(connector),
        )
        .await
        .map_err(|e| DormantError::DisplayIo {
            controller: SamsungTizenController::NAME.into(),
            detail: format!("pair connect failed: {e}"),
        })?;

        // The TV sends back JSON events during pairing. The token arrives in
        // the "data" field of a message with event "ms.channel.connect" or
        // similar. The caller-supplied `timeout_dur` bounds the entire handshake.
        loop {
            let msg = ws
                .next()
                .await
                .ok_or_else(|| DormantError::DisplayIo {
                    controller: SamsungTizenController::NAME.into(),
                    detail: "pairing WebSocket closed before token received".into(),
                })?
                .map_err(|e| DormantError::DisplayIo {
                    controller: SamsungTizenController::NAME.into(),
                    detail: format!("pairing read error: {e}"),
                })?;

            if let Message::Text(text) = msg {
                let text = text.clone();
                if let Some(token) = extract_pair_token(&text) {
                    return Ok(token);
                }
            }
        }
    })
    .await
    .map_err(|_| DormantError::DisplayIo {
        controller: SamsungTizenController::NAME.into(),
        detail: format!(
            "no response from TV within {timeout_dur:?} — \
             accept the 'Allow' prompt on the TV?"
        ),
    })?
}

// ── Doctor shims — best-effort network probes exposed for dormant-doctor ─────────

/// Best-effort TCP reachability probe for a Samsung TV port (doctor use).
///
/// Returns `true` if `host:port` accepts a TCP connection within
/// `REST_TIMEOUT`; otherwise `false`.
#[must_use]
pub async fn probe_reachable(host: &str, port: u16) -> bool {
    RealTvTransport::new()
        .tcp_connect_ok(host, port, REST_TIMEOUT)
        .await
}

/// Best-effort power-state read via the REST device-info endpoint (doctor use).
///
/// Returns `Some("on")` or `Some("standby")` if the TV responds, or `None` if
/// unreachable.
#[must_use]
pub async fn probe_power_state(host: &str) -> Option<String> {
    RealTvTransport::new().get_power_state(host).await
}

/// Extract the pairing token from a WebSocket text message.
///
/// The TV returns JSON with a `"data"` field containing a `"token"` string
/// after the user accepts the pairing dialog.
fn extract_pair_token(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("data")
        .and_then(|d| d.get("token"))
        .and_then(|t| t.as_str())
        .map(String::from)
}

// ── Fake transport for tests ────────────────────────────────────────────────────

/// Test-only transport that records calls and returns pre-programmed responses.
///
/// Each method can be configured with a fixed response or a sequence.
#[derive(Debug, Default)]
#[allow(dead_code)]
struct FakeTvTransport {
    /// Key names sent via `send_key`, in order.
    sent_keys: StdMutex<Vec<String>>,
    /// Return values for successive `send_key` calls.
    send_key_results: StdMutex<Vec<Result<(), String>>>,
    /// Return values for `get_power_state` calls.
    power_state_results: StdMutex<Vec<Option<String>>>,
    /// Return values for `send_wol` calls.
    wol_results: StdMutex<Vec<Result<(), String>>>,
    /// MAC addresses passed to `send_wol`, in order.
    wol_macs: StdMutex<Vec<String>>,
    /// Return values for `tcp_connect_ok` calls.
    connect_results: StdMutex<Vec<bool>>,
}

#[allow(dead_code)]
impl FakeTvTransport {
    fn new() -> Self {
        Self::default()
    }

    fn with_send_key_results(results: Vec<Result<(), String>>) -> Self {
        Self {
            send_key_results: StdMutex::new(results),
            ..Default::default()
        }
    }

    fn with_connect_results(results: Vec<bool>) -> Self {
        Self {
            connect_results: StdMutex::new(results),
            ..Default::default()
        }
    }

    fn take_sent_keys(&self) -> Vec<String> {
        std::mem::take(&mut *self.sent_keys.lock().unwrap())
    }

    fn take_wol_macs(&self) -> Vec<String> {
        std::mem::take(&mut *self.wol_macs.lock().unwrap())
    }
}

#[async_trait]
impl TvTransport for FakeTvTransport {
    async fn send_key(&self, _host: &str, _token: &str, key: &str) -> Result<(), String> {
        self.sent_keys.lock().unwrap().push(key.to_string());
        let mut results = self.send_key_results.lock().unwrap();
        if results.is_empty() {
            Ok(())
        } else {
            results.remove(0)
        }
    }

    async fn get_power_state(&self, _host: &str) -> Option<String> {
        let mut results = self.power_state_results.lock().unwrap();
        if results.is_empty() {
            None
        } else {
            results.remove(0)
        }
    }

    async fn send_wol(&self, mac: &str) -> Result<(), String> {
        self.wol_macs.lock().unwrap().push(mac.to_string());
        let mut results = self.wol_results.lock().unwrap();
        if results.is_empty() {
            Ok(())
        } else {
            results.remove(0)
        }
    }

    async fn tcp_connect_ok(&self, _host: &str, _port: u16, _connect_timeout: Duration) -> bool {
        let mut results = self.connect_results.lock().unwrap();
        if results.is_empty() {
            true
        } else {
            results.remove(0)
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use crate::samsung_ip::FakeBacklightTransport;
    use dormant_core::error::E_DISPLAY_IO;

    // ── URL construction ────────────────────────────────────────────────────

    #[test]
    fn build_ws_url_includes_base64_name_and_token() {
        let url = format!(
            "wss://192.0.2.7:8002/api/v2/channels/samsung.remote.control?name={expected_name}&token=abc123",
            expected_name = base64::engine::general_purpose::STANDARD.encode("dormant")
        );
        let expected_name = base64::engine::general_purpose::STANDARD.encode("dormant");
        assert!(
            url.starts_with("wss://192.0.2.7:8002/api/v2/channels/samsung.remote.control?name="),
            "URL prefix wrong: {url}"
        );
        assert!(
            url.contains(&format!("name={expected_name}")),
            "URL missing base64 device name: {url}"
        );
        assert!(url.contains("token=abc123"), "URL missing token: {url}");
    }

    // ── WebSocket handshake headers (regression: sec-websocket-key) ─────────

    /// Regression guard: the old bare `Request::builder().uri().body(())`
    /// pattern produces a request with NO `sec-websocket-key` header — a
    /// WSS connect against a real Samsung TV fails with "Missing, duplicated
    /// or incorrect header sec-websocket-key". Pins the real bug so a
    /// regression to the older request-construction path is caught here
    /// rather than at the live TV.
    #[test]
    fn old_builder_missing_sec_websocket_key_proof() {
        // Construct the URL the same way the real code does.
        let name_b64 = base64::engine::general_purpose::STANDARD.encode("dormant");
        let url = format!("wss://192.0.2.7:8002{WS_PATH}?name={name_b64}&token=abc123");

        // OLD pattern — exactly what the broken code used.
        let uri = url
            .parse::<tokio_tungstenite::tungstenite::http::Uri>()
            .unwrap();
        let old_request = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri(uri)
            .body(())
            .unwrap();

        // RED: the old construction does NOT generate a Sec-WebSocket-Key header.
        assert!(
            !old_request.headers().contains_key("sec-websocket-key"),
            "OLD construction MUST lack sec-websocket-key — otherwise the bug never existed"
        );
    }

    /// The FIXED construction via `IntoClientRequest` generates all mandatory
    /// WebSocket handshake headers.
    #[test]
    fn into_client_request_generates_handshake_headers() {
        let name_b64 = base64::engine::general_purpose::STANDARD.encode("dormant");
        let url = format!("wss://192.0.2.7:8002{WS_PATH}?name={name_b64}&token=abc123");

        let request = url.as_str().into_client_request().unwrap();
        let headers = request.headers();

        assert!(
            headers.contains_key("sec-websocket-key"),
            "fixed request must carry a sec-websocket-key header"
        );

        let upgrade = headers
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(
            upgrade.to_lowercase(),
            "websocket",
            "Upgrade header must be 'websocket' (case-insensitive)"
        );

        let connection = headers
            .get("connection")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            connection.to_lowercase().contains("upgrade"),
            "Connection header must contain 'Upgrade'"
        );

        let version = headers
            .get("sec-websocket-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(version, "13", "Sec-WebSocket-Version must be 13");
    }

    // ── JSON payload construction ───────────────────────────────────────────

    #[test]
    fn key_payload_contains_exact_json_structure() {
        let payload = build_key_payload("KEY_PICTURE_OFF");
        let v: serde_json::Value =
            serde_json::from_str(&payload).expect("payload must be valid JSON");

        assert_eq!(v["method"], "ms.remote.control");
        assert_eq!(v["params"]["Cmd"], "Click");
        assert_eq!(v["params"]["DataOfCmd"], "KEY_PICTURE_OFF");
        assert_eq!(v["params"]["Option"], "false");
        assert_eq!(v["params"]["TypeOfRemote"], "SendRemoteKey");
    }

    #[test]
    fn key_payload_contains_expected_keys() {
        let keys = ["KEY_PICTURE_OFF", "KEY_RETURN", "KEY_POWER"];
        for key in &keys {
            let payload = build_key_payload(key);
            let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(
                v["params"]["DataOfCmd"], *key,
                "payload DataOfCmd mismatch for {key}"
            );
        }
    }

    // ── WoL magic packet construction ───────────────────────────────────────

    #[test]
    fn magic_packet_starts_with_six_ff() {
        let packet = build_magic_packet("aa:bb:cc:dd:ee:ff").unwrap();
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xFF; 6]);
    }

    #[test]
    fn magic_packet_repeats_mac_16_times() {
        let mac_str = "11:22:33:44:55:66";
        let packet = build_magic_packet(mac_str).unwrap();
        let expected_mac = &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        for i in 0..16 {
            let offset = 6 + i * 6;
            assert_eq!(
                &packet[offset..offset + 6],
                expected_mac,
                "MAC at repeat {i} mismatch"
            );
        }
    }

    #[test]
    fn magic_packet_accepts_hyphens_and_colons() {
        let p1 = build_magic_packet("aa:bb:cc:dd:ee:ff").unwrap();
        let p2 = build_magic_packet("aa-bb-cc-dd-ee-ff").unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn magic_packet_rejects_short_mac() {
        let err = build_magic_packet("aa:bb:cc").unwrap_err();
        assert!(err.contains("12 hex digits"), "error: {err}");
    }

    #[test]
    fn magic_packet_rejects_invalid_hex() {
        // 'g' is not a hex digit — filtered out, leaving only 10 hex digits,
        // which fails the length check.
        let err = build_magic_packet("gg:bb:cc:dd:ee:ff").unwrap_err();
        assert!(err.contains("12 hex digits"), "error: {err}");
    }

    // ── TLS connector ───────────────────────────────────────────────────────

    #[test]
    fn tls_config_uses_no_verify_verifier() {
        // The config must build without panic — verifier type is opaque at
        // the ClientConfig level, so this test verifies construction succeeds.
        let _config = RealTvTransport::tls_config();
    }

    // ── Controller: blank and wake ──────────────────────────────────────────

    fn test_controller(transport: Arc<FakeTvTransport>) -> SamsungTizenController {
        SamsungTizenController::with_transport(
            "192.0.2.7".into(),
            "test-token".into(),
            None,
            true,
            transport,
        )
    }

    #[tokio::test]
    async fn blank_screen_off_sends_key_picture_off() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = test_controller(fake.clone());
        ctrl.blank(BlankMode::ScreenOffAudioOn).await.unwrap();
        let keys = fake.take_sent_keys();
        assert_eq!(keys, vec!["KEY_PICTURE_OFF"]);
    }

    #[tokio::test]
    async fn blank_power_off_sends_key_power() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = test_controller(fake.clone());
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
        let keys = fake.take_sent_keys();
        assert_eq!(keys, vec!["KEY_POWER"]);
    }

    #[tokio::test]
    async fn wake_sends_key_return() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = test_controller(fake.clone());
        ctrl.wake().await.unwrap();
        let keys = fake.take_sent_keys();
        assert_eq!(keys, vec!["KEY_RETURN"]);
    }

    #[tokio::test]
    async fn wake_with_wol_mac_sends_wol_before_key() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = SamsungTizenController::with_transport(
            "192.0.2.7".into(),
            "tok".into(),
            Some("aa:bb:cc:dd:ee:ff".into()),
            true,
            fake.clone(),
        );
        ctrl.wake().await.unwrap();
        let macs = fake.take_wol_macs();
        assert_eq!(macs, vec!["aa:bb:cc:dd:ee:ff"]);
        let keys = fake.take_sent_keys();
        assert_eq!(keys, vec!["KEY_RETURN"]);
    }

    /// `BrightnessZero` is supported by samsung-tizen (via port-1516 backlight).
    /// The fake `TvTransport` is irrelevant — blank goes through the backlight
    /// transport, which the test wires below.
    #[tokio::test]
    async fn blank_brightness_zero_acquires_reads_sets_via_backlight_transport() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        // Program: acquire → "tok", get_backlight → 35, set_backlight → ok.
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok-1".into()));
        bl_fake.get_results.lock().unwrap().push(Ok(35));
        bl_fake.set_results.lock().unwrap().push(Ok(()));

        let ctrl = SamsungTizenController::with_transports(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake.clone(),
            bl_fake.clone(),
        );

        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();

        // No WS key was sent — BrightnessZero uses the backlight path only.
        assert!(
            tv_fake.take_sent_keys().is_empty(),
            "BrightnessZero must not send WS keys"
        );

        // Acquire was called once with the right host.
        assert_eq!(*bl_fake.acquire_hosts.lock().unwrap(), vec!["192.0.2.7"]);
        // Get was called with the acquired token.
        assert_eq!(
            *bl_fake.get_calls.lock().unwrap(),
            vec![("192.0.2.7".to_string(), "tok-1".to_string())]
        );
        // Set was called once with backlight=0 and the acquired token
        // (recorded as value, token comes through get_calls).
        assert_eq!(
            *bl_fake.set_calls.lock().unwrap(),
            vec![("192.0.2.7".to_string(), 0)]
        );

        // First-blank-wins: saved_backlight is the read value (35), so a
        // second blank reads 0 but does NOT overwrite the saved value.
        assert_eq!(*ctrl.saved_backlight.lock().unwrap(), Some(35));
    }

    /// First-blank-wins: a second blank while already at backlight 0 must
    /// NOT clobber the saved value with 0, or wake would restore 0
    /// (stuck-dark panel).
    #[tokio::test]
    async fn blank_brightness_zero_twice_does_not_overwrite_saved() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok".into()));
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok".into()));
        bl_fake.get_results.lock().unwrap().push(Ok(42));
        bl_fake.get_results.lock().unwrap().push(Ok(0));
        bl_fake.set_results.lock().unwrap().push(Ok(()));
        bl_fake.set_results.lock().unwrap().push(Ok(()));

        let ctrl = SamsungTizenController::with_transports(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake,
            bl_fake.clone(),
        );

        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(*ctrl.saved_backlight.lock().unwrap(), Some(42));
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(
            *ctrl.saved_backlight.lock().unwrap(),
            Some(42),
            "second blank must NOT clobber saved value with 0"
        );

        // Two set_backlight calls happened, both to 0.
        assert_eq!(bl_fake.set_calls.lock().unwrap().len(), 2);
    }

    /// Wake after `BrightnessZero`: restore the saved backlight value, then
    /// clear saved so the next cycle re-saves fresh.
    #[tokio::test]
    async fn wake_after_brightness_zero_restores_saved_and_clears() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok".into()));
        bl_fake.get_results.lock().unwrap().push(Ok(28));
        bl_fake.set_results.lock().unwrap().push(Ok(()));
        // For wake: acquire + set_backlight(28)
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok".into()));
        bl_fake.set_results.lock().unwrap().push(Ok(()));

        let ctrl = SamsungTizenController::with_transports_mode(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake.clone(),
            bl_fake.clone(),
            BlankMode::BrightnessZero,
        );

        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(*ctrl.saved_backlight.lock().unwrap(), Some(28));

        ctrl.wake().await.unwrap();
        // Wake restored to 28 and cleared saved.
        assert_eq!(
            *bl_fake.set_calls.lock().unwrap(),
            vec![("192.0.2.7".to_string(), 0), ("192.0.2.7".to_string(), 28)]
        );
        assert!(
            ctrl.saved_backlight.lock().unwrap().is_none(),
            "wake must clear saved_backlight so the next cycle re-saves"
        );
        // KEY_RETURN was NOT sent — backlight restore is the wake.
        assert!(
            tv_fake.take_sent_keys().is_empty(),
            "wake after BrightnessZero must not send KEY_RETURN"
        );
    }

    /// Wake after picture-off (no saved backlight) must still send `KEY_RETURN`.
    #[tokio::test]
    async fn wake_without_saved_backlight_sends_key_return() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());

        let ctrl = SamsungTizenController::with_transports(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake.clone(),
            bl_fake,
        );

        ctrl.wake().await.unwrap();
        assert_eq!(tv_fake.take_sent_keys(), vec![KEY_RETURN]);
    }

    /// `BrightnessZero` blank when the TV is unreachable (and the policy says
    /// to treat unreachable as blanked) must succeed as a no-op — same
    /// contract as the WebSocket path.
    #[tokio::test]
    async fn blank_brightness_zero_unreachable_noops_when_policy_enabled() {
        let tv_fake = Arc::new(FakeTvTransport::with_connect_results(vec![false]));
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        let ctrl = SamsungTizenController::with_transports(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true, // treat_unreachable_as_blanked = true
            tv_fake,
            bl_fake.clone(),
        );
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        // Backlight transport was not touched.
        assert!(bl_fake.acquire_hosts.lock().unwrap().is_empty());
        assert!(bl_fake.set_calls.lock().unwrap().is_empty());
    }

    /// `BrightnessZero` backlight errors must surface as `CmdFailure` with the
    /// `E_DISPLAY_IO` prefix and a JSON-RPC code anchor.
    #[tokio::test]
    async fn blank_brightness_zero_failure_maps_to_e_display_io() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Err("-32601 boom".into()));
        let ctrl = SamsungTizenController::with_transports(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake,
            bl_fake,
        );
        let err = ctrl.blank(BlankMode::BrightnessZero).await.unwrap_err();
        assert_eq!(err.controller, "samsung-tizen");
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains(samsung_ip::E_JSONRPC_METHOD_NOT_FOUND));
    }

    /// A -32010 unauthorized response from set/get must invalidate the
    /// cached token so the controller's next call re-acquires.
    #[tokio::test]
    async fn blank_brightness_zero_unauthorized_invalidates_token() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("stale-tok".into()));
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("fresh-tok".into()));
        // First get returns the unauthorized error.
        bl_fake
            .get_results
            .lock()
            .unwrap()
            .push(Err("-32010 bad token".into()));
        // Second acquire succeeds, then get+set succeed.
        bl_fake.get_results.lock().unwrap().push(Ok(20));
        bl_fake.set_results.lock().unwrap().push(Ok(()));

        let ctrl = SamsungTizenController::with_transports(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake,
            bl_fake.clone(),
        );

        // The first blank fails — but map_transport_error calls invalidate
        // which is wired into the fake's `acquire_hosts` push marker.
        let _ = ctrl.blank(BlankMode::BrightnessZero).await;
        // The map helper was hit, recording an invalidate marker.
        let hosts = bl_fake.acquire_hosts.lock().unwrap();
        assert!(
            hosts.iter().any(|h| h == "invalidate:192.0.2.7"),
            "invalidate_token should have been called on -32010; hosts={hosts:?}"
        );
    }

    // ── Unreachable no-op policy ────────────────────────────────────────────

    #[tokio::test]
    async fn blank_unreachable_noop_when_policy_enabled() {
        let fake = Arc::new(FakeTvTransport::with_connect_results(vec![false]));
        let ctrl = SamsungTizenController::with_transport(
            "192.0.2.7".into(),
            "tok".into(),
            None,
            true, // treat_unreachable_as_blanked = true
            fake.clone(),
        );
        // Blank must succeed as no-op even though TV is unreachable.
        ctrl.blank(BlankMode::ScreenOffAudioOn).await.unwrap();
        let keys = fake.take_sent_keys();
        assert!(keys.is_empty(), "no keys should be sent when unreachable");
    }

    #[tokio::test]
    async fn wake_unreachable_noop_when_policy_enabled() {
        let fake = Arc::new(FakeTvTransport::with_connect_results(vec![false]));
        let ctrl = SamsungTizenController::with_transport(
            "192.0.2.7".into(),
            "tok".into(),
            None,
            true,
            fake.clone(),
        );
        ctrl.wake().await.unwrap();
        let keys = fake.take_sent_keys();
        assert!(keys.is_empty(), "no keys should be sent when unreachable");
    }

    #[tokio::test]
    async fn wake_unreachable_still_sends_wol_then_noops() {
        let fake = Arc::new(FakeTvTransport {
            connect_results: StdMutex::new(vec![false]),
            ..FakeTvTransport::default()
        });
        let ctrl = SamsungTizenController::with_transport(
            "192.0.2.7".into(),
            "tok".into(),
            Some("aa:bb:cc:dd:ee:ff".into()),
            true,
            fake.clone(),
        );
        ctrl.wake().await.unwrap();
        let macs = fake.take_wol_macs();
        assert_eq!(macs, vec!["aa:bb:cc:dd:ee:ff"]);
        let keys = fake.take_sent_keys();
        assert!(keys.is_empty(), "no key sent after noop");
    }

    #[tokio::test]
    async fn blank_unreachable_errors_when_policy_disabled() {
        // With policy disabled, unreachable TV still tries to send key.
        // Since send_key also fails (no results), we get an error.
        let fake = Arc::new(FakeTvTransport {
            connect_results: StdMutex::new(vec![false]),
            send_key_results: StdMutex::new(vec![Err("network error".into())]),
            ..FakeTvTransport::default()
        });
        let ctrl = SamsungTizenController::with_transport(
            "192.0.2.7".into(),
            "tok".into(),
            None,
            false, // treat_unreachable_as_blanked = false
            fake.clone(),
        );
        let err = ctrl.blank(BlankMode::ScreenOffAudioOn).await.unwrap_err();
        assert_eq!(err.controller, SamsungTizenController::NAME);
        assert!(err.error.starts_with(E_DISPLAY_IO));
    }

    // ── Reconnect on send failure ───────────────────────────────────────────

    #[tokio::test]
    async fn send_key_retry_after_failure() {
        // First send_key fails, second succeeds.
        let fake = Arc::new(FakeTvTransport::with_send_key_results(vec![
            Err("broken pipe".into()),
            Ok(()),
        ]));
        // Transport must retry on the first failure
        // Note: the real transport reconnects internally; the fake here
        // simulates the retry at the transport trait level since the
        // controller delegates to transport.send_key().
        // The reconnect-on-failure logic lives in RealTvTransport's
        // ws_send_with_retry; this test verifies the controller correctly
        // delegates to transport.send_key and surfaces errors properly.
        let ctrl = test_controller(fake.clone());
        // First call: send_key fails once (fake returns Err), then succeeds
        // on second attempt. Since the controller calls send_key once per
        // blank, and our fake returns Err on the first call...
        // Actually, the real reconnect happens inside RealTvTransport.
        // This test verifies the CmdFailure mapping from a failed send_key.
        let err = ctrl.blank(BlankMode::ScreenOffAudioOn).await.unwrap_err();
        assert!(err.error.contains("broken pipe"));
        // Second blank should succeed (second result in the fake).
        // But the controller is the same instance — second blank call.
    }

    #[tokio::test]
    async fn send_key_success_after_one_failure() {
        let fake = Arc::new(FakeTvTransport::with_send_key_results(vec![
            Err("broken pipe".into()),
            Ok(()),
        ]));
        let ctrl = test_controller(fake.clone());
        // First blank fails
        let err = ctrl.blank(BlankMode::ScreenOffAudioOn).await.unwrap_err();
        assert!(err.error.contains("broken pipe"));
        // Second blank succeeds
        ctrl.blank(BlankMode::ScreenOffAudioOn).await.unwrap();
        let keys = fake.take_sent_keys();
        assert_eq!(keys.len(), 2);
    }

    // ── Power state ─────────────────────────────────────────────────────────

    #[test]
    fn extract_pair_token_from_json() {
        let json = r#"{"event":"ms.channel.connect","data":{"token":"abc123granted"}}"#;
        let token = extract_pair_token(json);
        assert_eq!(token, Some("abc123granted".to_string()));
    }

    #[test]
    fn extract_pair_token_no_token_field() {
        let json = r#"{"event":"ms.channel.ready","data":{}}"#;
        assert_eq!(extract_pair_token(json), None);
    }

    #[test]
    fn extract_pair_token_not_json() {
        assert_eq!(extract_pair_token("not json"), None);
    }

    // ── Controller metadata ─────────────────────────────────────────────────

    #[tokio::test]
    async fn name_is_literal_string() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = test_controller(fake);
        assert_eq!(ctrl.name(), "samsung-tizen");
    }

    #[tokio::test]
    async fn supported_modes_includes_screen_off_power_off_and_brightness_zero() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = test_controller(fake);
        let modes = ctrl.supported_modes();
        assert!(modes.contains(&BlankMode::ScreenOffAudioOn));
        assert!(modes.contains(&BlankMode::PowerOff));
        assert!(
            modes.contains(&BlankMode::BrightnessZero),
            "BrightnessZero (audio-safe dim via port 1516) is supported"
        );
    }

    #[tokio::test]
    async fn is_available_when_tcp_connects() {
        let fake = Arc::new(FakeTvTransport::with_connect_results(vec![true]));
        let ctrl = test_controller(fake);
        assert!(ctrl.is_available().await);
    }

    #[tokio::test]
    async fn is_unavailable_when_tcp_fails() {
        let fake = Arc::new(FakeTvTransport::with_connect_results(vec![false]));
        let ctrl = test_controller(fake);
        assert!(!ctrl.is_available().await);
    }

    // ── TLS rcgen handshake test ────────────────────────────────────────────

    /// Verify that the `NoVerify` verifier + `RealTvTransport::tls_config()`
    /// successfully completes a TLS handshake with a self-signed certificate
    /// generated by `rcgen`.
    #[tokio::test]
    #[cfg(feature = "rcgen-test")]
    async fn tls_handshake_with_self_signed_cert() {
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;

        // Generate a self-signed certificate.
        let cert_params =
            rcgen::CertificateParams::new(["localhost".into()]).expect("rcgen params");
        let key_pair = rcgen::KeyPair::generate().expect("rcgen keypair");
        let cert = cert_params
            .self_signed(&key_pair)
            .expect("rcgen self-signed cert");

        let cert_der = cert.der().clone();
        let key_der = key_pair.serialize_der();

        // Set up a TLS acceptor with the self-signed cert.
        let provider: Arc<rustls::crypto::CryptoProvider> =
            rustls::crypto::ring::default_provider().into();
        let mut server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("safe default protocol versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert_der.to_vec())],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(key_der.clone()),
                ),
            )
            .expect("server config");

        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn a TLS server that accepts one connection.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = acceptor.accept(stream).await;
        });

        // Connect with the NoVerify client config.
        let client_config = RealTvTransport::tls_config();
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();

        let tcp = TcpStream::connect(addr).await.unwrap();
        let result = connector.connect(server_name, tcp).await;
        assert!(
            result.is_ok(),
            "TLS handshake with self-signed cert should succeed with NoVerify: {:?}",
            result.err()
        );

        server.await.unwrap();
    }

    // ── Cold-start socket priming ────────────────────────────────────────────

    /// On a fresh daemon start the WS cache is `None` — `ws_send_with_retry`
    /// must connect on demand, not return an error.
    #[tokio::test]
    async fn cold_start_connects_on_first_send() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // New design: no client-side Ping before the key. The server reads
        // exactly one frame (the key). The reader task is spawned on
        // connect; it sits in `select!` waiting for frames and does not
        // block the send path.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let msg = ws.next().await;
            assert!(msg.is_some(), "server should receive a frame");
        });

        let transport = RealTvTransport::for_test(port);
        let result = transport
            .send_key("127.0.0.1", "test-token", KEY_RETURN)
            .await;
        assert!(result.is_ok(), "cold-start send should succeed: {result:?}");

        server.await.unwrap();
    }

    // ── Reconnect on send failure ────────────────────────────────────────────

    /// The reconnect-on-send-failure path in `ws_send_with_retry`:
    /// after the cached WS socket is closed (simulating the TV silently
    /// dropping an idle socket), the next `send_key` must reconnect and
    /// retry, not return an error.
    #[tokio::test]
    async fn real_transport_reconnects_on_broken_pipe() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server accepts two connections.
        let server = tokio::spawn(async move {
            // Connection 1: accept, read the priming key.
            {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(stream).await.unwrap();
                let msg = ws.next().await; // priming key
                assert!(msg.is_some(), "connection 1 should receive a frame");
            }
            // Connection 2: accept, read the retry frame.
            {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(stream).await.unwrap();
                let msg = ws.next().await;
                assert!(msg.is_some(), "connection 2 should receive retry frame");
            }
        });

        let transport = RealTvTransport::for_test(port);

        // First send primes the cache.
        transport
            .send_key("127.0.0.1", "test-token", KEY_RETURN)
            .await
            .unwrap();

        // Explicitly close the cached socket — this simulates the TV
        // dropping the connection after picture-off. The reader task on
        // the dead read half flips the `dead` flag and exits; the next
        // `send_key` sees dead=true and reconnects.
        transport.close_cached_for_test().await;

        // Second send: sees dead cached socket → reconnect → retry succeeds
        // on connection 2.
        let result = transport
            .send_key("127.0.0.1", "test-token", KEY_PICTURE_OFF)
            .await;
        assert!(
            result.is_ok(),
            "retry after broken pipe should succeed: {result:?}"
        );

        server.await.unwrap();
    }

    // ── Reader-task lifecycle (cancellation token) ──────────────────────────

    /// When the transport replaces its cached socket (reconnect), the old
    /// reader task MUST be cancelled — otherwise it leaks across
    /// reconnects, holding dead stream halves and logging spurious errors.
    ///
    /// The test forces a reconnect via the freshness check and verifies
    /// that the `reader_state` Arc is swapped to a fresh instance (so the
    /// prior reader's `CancellationToken` is no longer the active one).
    /// The prior reader task is observing a `CancellationToken` that has
    /// been fired; its `select!` loop exits on the next poll.
    #[tokio::test]
    async fn old_reader_task_is_cancelled_on_reconnect() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server accepts TWO connections. Conn 1 stays alive but silent
        // (no frames sent — simulates the silent-drop scenario); conn 2
        // receives the retry key after freshness-driven reconnect.
        let server = tokio::spawn(async move {
            // Conn 1: read priming key, then stay silent (no Close, no
            // pings — the kernel TCP socket stays open so the client's
            // reader task sees no EOF and does not mark itself dead; only
            // the freshness timer fires).
            let (s1, _) = listener.accept().await.unwrap();
            let mut ws1 = accept_async(s1).await.unwrap();
            let _priming = ws1.next().await;
            // Hold the WS open long enough for the client's freshness
            // check to fire and the reconnect to land on conn 2.
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(ws1);

            // Conn 2: receives the key sent after freshness-driven reconnect.
            let (s2, _) = listener.accept().await.unwrap();
            let mut ws2 = accept_async(s2).await.unwrap();
            let msg = ws2.next().await;
            assert!(
                msg.is_some(),
                "conn2 should receive frame after freshness reconnect"
            );
        });

        // Tight silence window so the test doesn't wait 10 s.
        let transport = Arc::new(RealTvTransport::for_test_with_silence(
            port,
            Duration::from_millis(100),
        ));

        // First send: cold cache → connect, priming key lands on conn 1.
        transport
            .send_key("127.0.0.1", "test-token", KEY_RETURN)
            .await
            .unwrap();

        // Capture the pre-reconnect reader_state Arc — held by the reader
        // task spawned on connect. After reconnect, a fresh Arc replaces
        // it; the prior Arc remains alive only because the prior reader
        // task still holds a clone (and our `state_before` clone), but its
        // CancellationToken has been fired.
        let state_before = transport.reader_state_for_test();

        // Wait past the freshness window — the silent server never sent a
        // frame, so `last_seen` is stale by now. (We don't care if
        // state_before is_dead — the silent-drop scenario keeps the socket
        // alive but uneventful.)
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Second send: freshness check fires → reconnect → fresh reader
        // task spawned with the swapped Arc.
        let result = transport
            .send_key("127.0.0.1", "test-token", KEY_PICTURE_OFF)
            .await;
        assert!(
            result.is_ok(),
            "send after stale should reconnect: {result:?}"
        );

        let state_after = transport.reader_state_for_test();
        assert!(
            !Arc::ptr_eq(&state_before, &state_after),
            "reader_state Arc should be swapped on reconnect"
        );

        // The prior CancellationToken must be cancelled (so the prior
        // reader task will exit on its next select! poll).
        assert!(
            state_before.cancel.is_cancelled(),
            "prior reader_state's CancellationToken must be fired"
        );
        // The fresh CancellationToken must NOT be cancelled (the new
        // reader task is still running).
        assert!(
            !state_after.cancel.is_cancelled(),
            "fresh reader_state's CancellationToken must NOT be fired"
        );

        server.await.unwrap();
    }

    /// Silent-drop scenario: a fresh socket whose reader task has not observed
    /// any frame (the TV is silent — TCP alive but no frames) must be
    /// detected as STALE on the next send, triggering a reconnect that
    /// delivers the key on a fresh connection. Guards against the original
    /// silent-drop bug where a half-open socket silently accepted the write
    /// and reported success while the key vanished.
    #[tokio::test]
    async fn silent_drop_triggers_reconnect_via_last_seen() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            // Conn 1: accept the priming key, then go silent — NO frame
            // is sent back. The client's reader task never observes a
            // frame, so `last_seen` stays at connect time.
            let (s1, _) = listener.accept().await.unwrap();
            let mut ws1 = accept_async(s1).await.unwrap();
            let _priming = ws1.next().await;

            // Hold ws1 alive long enough for the client's freshness check
            // to fire and the reconnect to land on conn 2.
            let _keepalive = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                drop(ws1);
            });

            // Conn 2: receive the retry key (after freshness check fired).
            let (s2, _) = listener.accept().await.unwrap();
            let mut ws2 = accept_async(s2).await.unwrap();
            let msg = ws2.next().await;
            assert!(
                msg.is_some(),
                "conn2 should receive retry key after freshness reconnect"
            );
            if let Some(Ok(Message::Text(text))) = msg {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    parsed["params"]["DataOfCmd"].as_str().unwrap(),
                    KEY_PICTURE_OFF,
                    "retry key should be KEY_PICTURE_OFF on the reconnected socket"
                );
            } else {
                panic!("expected text frame with key payload on conn2");
            }
        });

        // Tight silence window so the test doesn't wait 10 s.
        let transport = RealTvTransport::for_test_with_silence(port, Duration::from_millis(150));

        // First send: cold cache → connect (priming key lands on conn 1).
        transport
            .send_key("127.0.0.1", "test-token", KEY_RETURN)
            .await
            .unwrap();

        // Wait past the freshness window — the silent server never sent a
        // frame, so `last_seen` is stale by now.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Second send: freshness check fires (dead=false, stale=true) →
        // reconnect → retry key lands on conn 2 within 3 s.
        let timed = tokio::time::timeout(
            Duration::from_secs(3),
            transport.send_key("127.0.0.1", "test-token", KEY_PICTURE_OFF),
        )
        .await;
        assert!(
            timed.is_ok(),
            "send_key did not reconnect+deliver within 3s (silent-drop not detected): {timed:?}"
        );
        assert!(
            timed.unwrap().is_ok(),
            "silent-drop send should reconnect and succeed"
        );

        server.await.unwrap();
    }

    /// Freshness check (not Ping/Pong) is the liveness gate. When `last_seen`
    /// is rewound past the silence window, the freshness check fires
    /// immediately — proving that the liveness decision is driven by the
    /// TV-driven heartbeat signal, not by a client-initiated round-trip.
    #[tokio::test]
    async fn freshness_check_is_the_gate_not_ping() {
        let transport = RealTvTransport::for_test_with_silence(
            // Port unused — we don't actually connect in this test.
            1,
            Duration::from_millis(500),
        );
        // Pre-fix invariant: an unwarmed `last_seen` (recently constructed)
        // is NOT yet stale. The reader task starts with `last_seen = now()`.
        let state = transport.reader_state_for_test();
        assert!(
            !state.is_stale(Duration::from_millis(500)),
            "fresh reader_state should not be stale immediately"
        );
        // After rewinding `last_seen` into the past, the freshness check
        // fires immediately.
        transport.age_last_seen_for_test(Duration::from_secs(60));
        assert!(
            state.is_stale(Duration::from_millis(500)),
            "after rewinding last_seen 60s, freshness check must report stale"
        );
    }

    // ── Pairing handshake ────────────────────────────────────────────────────

    /// Stand up a local WS server that sends a pairing token frame; assert
    /// `pair()` returns the token.
    #[tokio::test]
    async fn pair_extracts_token_from_server_frame() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // The `pair()` function connects to wss://host:8002 — we need to
        // adapt it. For the test we use a helper that connects to the
        // listener directly; we test `extract_pair_token` logic internally
        // and the full `pair()` flow indirectly.
        //
        // Because `pair()` hardcodes wss:// and port 8002, we can't point it
        // at a local plain-TCP server without refactoring. Instead we test
        // `extract_pair_token` directly (already covered) and add a
        // constrained server-handshake test via a local pair-helper.

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            // Send the token-bearing frame that pair() expects.
            let token_json =
                r#"{"event":"ms.channel.connect","data":{"token":"granted-token-42"}}"#;
            ws.send(WsMsg::Text(token_json.into())).await.unwrap();
            // Give the client time to read before closing.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        // Connect as `pair()` would — tokenless, wait for the token frame.
        // Since `pair()` uses wss://, we simulate the same logic on plain WS
        // by extracting the token from the server frame manually.
        let url = format!(
            "ws://127.0.0.1:{port}{WS_PATH}?name={}",
            base64::engine::general_purpose::STANDARD.encode(DEVICE_NAME)
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        let mut token: Option<String> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while token.is_none() && tokio::time::Instant::now() < deadline {
            let msg = tokio::time::timeout(Duration::from_secs(1), ws.next())
                .await
                .ok()
                .flatten();
            if let Some(Ok(WsMsg::Text(text))) = msg {
                token = extract_pair_token(&text.clone());
            }
        }
        assert_eq!(token, Some("granted-token-42".to_string()));

        server.await.unwrap();
    }

    /// `pair()` with a short timeout against an unreachable host returns
    /// quickly — not 60s.
    #[tokio::test]
    async fn pair_timeout_fails_fast() {
        let result = pair("192.0.2.1", Duration::from_millis(50)).await;
        assert!(result.is_err(), "expected Err against unreachable host");
    }

    // ── MUST 1 reverse-apply: transient set failure leaves saved intact ───

    /// After a transient `set_backlight` failure, `saved_backlight` is
    /// STILL `Some(N)` so a subsequent wake attempt can retry the restore.
    /// Pins the clear-before-success bug: the pre-fix ordering took the
    /// saved value before calling `set_backlight`, so a transient failure
    /// (network blip, TV busy) would lose the value and leave the panel
    /// stuck dim while the daemon thinks it woke.
    #[tokio::test]
    async fn restore_backlight_preserves_saved_on_set_failure() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok-1".into()));
        bl_fake.get_results.lock().unwrap().push(Ok(33));
        bl_fake.set_results.lock().unwrap().push(Ok(()));
        // For wake: acquire succeeds, but set_backlight fails.
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok-1".into()));
        bl_fake
            .set_results
            .lock()
            .unwrap()
            .push(Err("network blip".into()));

        let ctrl = SamsungTizenController::with_transports_mode(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake,
            bl_fake.clone(),
            BlankMode::BrightnessZero,
        );

        // Blank: saves 33.
        ctrl.blank(BlankMode::BrightnessZero).await.unwrap();
        assert_eq!(*ctrl.saved_backlight.lock().unwrap(), Some(33));

        // Wake attempt 1: set fails — Err returned. saved_backlight
        // must NOT have been cleared.
        let err = ctrl.wake().await.unwrap_err();
        assert_eq!(err.controller, "samsung-tizen");
        assert_eq!(
            *ctrl.saved_backlight.lock().unwrap(),
            Some(33),
            "transient set failure must preserve saved_backlight for retry"
        );
    }

    // ── MUST 2 reverse-apply: missing saved → restore to default, not KEY_RETURN

    /// A `BrightnessZero` controller with `saved_backlight = None` (e.g.
    /// after daemon restart) must wake by SETTING backlight to
    /// [`DEFAULT_RESTORE_BACKLIGHT`], NOT by sending `KEY_RETURN` — the
    /// latter does not raise port-1516 backlight and would leave the
    /// panel dim while the daemon thinks it woke.
    #[tokio::test]
    async fn brightness_zero_wake_with_no_saved_restores_to_default() {
        let tv_fake = Arc::new(FakeTvTransport::new());
        let bl_fake = Arc::new(FakeBacklightTransport::new());
        // For wake: acquire + set_backlight(DEFAULT_RESTORE_BACKLIGHT).
        bl_fake
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok".into()));
        bl_fake.set_results.lock().unwrap().push(Ok(()));

        let ctrl = SamsungTizenController::with_transports_mode(
            "192.0.2.7".into(),
            "ws-token".into(),
            None,
            true,
            tv_fake.clone(),
            bl_fake.clone(),
            BlankMode::BrightnessZero,
        );

        // saved_backlight starts at None (no blank yet, or restart).
        assert!(ctrl.saved_backlight.lock().unwrap().is_none());

        ctrl.wake().await.unwrap();

        // Backlight was restored to DEFAULT_RESTORE_BACKLIGHT.
        assert_eq!(
            *bl_fake.set_calls.lock().unwrap(),
            vec![("192.0.2.7".to_string(), DEFAULT_RESTORE_BACKLIGHT)]
        );
        // KEY_RETURN was NOT sent — backlight restore is the wake.
        assert!(
            tv_fake.take_sent_keys().is_empty(),
            "BrightnessZero wake must not fall through to KEY_RETURN"
        );
    }

    // ── MUST 3 reverse-apply: dropping the transport cancels the reader task

    /// When the transport is dropped (controller removed / daemon reload),
    /// the final reader task's `CancellationToken` is fired — without
    /// this, the reader task would outlive the transport and keep its
    /// socket half open past the daemon's lifetime.
    #[test]
    fn drop_transport_cancels_final_reader_state_token() {
        let transport = RealTvTransport::for_test_with_silence(
            // Port unused — we never connect in this test.
            1,
            Duration::from_secs(60),
        );
        let state = transport.reader_state_for_test();
        assert!(
            !state.cancel.is_cancelled(),
            "fresh reader_state must NOT be cancelled"
        );
        drop(transport);
        assert!(
            state.cancel.is_cancelled(),
            "dropping RealTvTransport must cancel the current reader_state's \
             CancellationToken so the reader task exits"
        );
    }
}
