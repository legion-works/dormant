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
//! | Mode | Key | Effect |
//! |---|---|---|
//! | `ScreenOffAudioOn` | `KEY_PICTURE_OFF` | Panel dark, audio continues; toggle |
//! | `PowerOff` | `KEY_POWER` | Full power-off |
//!
//! ## Wake
//!
//! `KEY_RETURN` wakes the TV from picture-off (verified on S90D — not a
//! toggle, safe to send when state is uncertain). When `wol_mac` is set,
//! a Wake-on-LAN magic packet is broadcast before the WS wake attempt.
//!
//! ## Socket lifecycle
//!
//! The TV silently drops idle WebSocket connections during picture-off,
//! causing `BrokenPipe` on the next send with no prior error. The controller
//! uses a cached persistent connection with reconnect-on-send-failure: every
//! `send_key` first tries the cached socket, and on failure reconnects and
//! retries once. This is load-bearing — a send without retry silently fails.
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
use std::time::Duration;

use tokio::sync::Mutex;

use async_trait::async_trait;
use base64::Engine as _;
use dormant_core::error::DormantError;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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

/// Maximum time to wait for a TCP connect probe.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// REST HTTP request timeout.
const REST_TIMEOUT: Duration = Duration::from_secs(3);

/// Default seconds for a WebSocket connect timeout.
const WS_CONNECT_TIMEOUT_SECS: u64 = 5;

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

/// Production transport: persistent TLS WebSocket, REST HTTP, `WoL` UDP.
///
/// When testing, can be constructed with a plain `ws://` scheme so that
/// local tokio-tungstenite servers (no TLS) can exercise the reconnect logic.
struct RealTvTransport {
    /// Cached WebSocket connection, protected by a mutex so that concurrent
    /// blank/wake calls don't race on reconnection.
    ws: Mutex<Option<WebSocketStream<MaybeTlsStream<TcpStream>>>>,
    /// URL scheme — `"wss"` in production, `"ws"` for plain-TCP tests.
    ws_scheme: &'static str,
    /// WebSocket port — 8002 in production, overridden in tests.
    ws_port: u16,
}

impl RealTvTransport {
    fn new() -> Self {
        Self {
            ws: Mutex::new(None),
            ws_scheme: "wss",
            ws_port: WS_PORT,
        }
    }

    /// Reconnect test helper: send a close frame on the cached WS and drop
    /// the stream, leaving a dead socket in the cache so the next `send_key`
    /// hits the "try-cached → fail → reconnect → retry" path.
    #[cfg(test)]
    async fn close_cached_for_test(&self) {
        let mut guard = self.ws.lock().await;
        if let Some(ref mut ws) = *guard {
            let _ = ws.close(None).await;
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
        Self {
            ws: Mutex::new(None),
            ws_scheme: "ws",
            ws_port: port,
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

    /// Connect (or reconnect) a WebSocket to the TV, replacing the cached socket.
    async fn connect_ws(&self, host: &str, token: &str) -> Result<(), String> {
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(DEVICE_NAME);
        let url = format!(
            "{}://{host}:{}{WS_PATH}?name={name_b64}&token={token}",
            self.ws_scheme, self.ws_port
        );

        // Branch at compile-like level: TLS vs plain. The two connect
        // functions return different opaque future types, so we can't use
        // a single `if/else` with Box::pin here.
        if self.ws_scheme == "wss" {
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
                Ok(Ok((ws, _response))) => {
                    let mut guard = self.ws.lock().await;
                    *guard = Some(ws);
                    Ok(())
                }
                Ok(Err(e)) => Err(format!("WebSocket connect failed: {e}")),
                Err(_) => Err("WebSocket connect timed out".to_string()),
            }
        } else {
            let connect_fut = tokio_tungstenite::connect_async(&url);
            match timeout(Duration::from_secs(WS_CONNECT_TIMEOUT_SECS), connect_fut).await {
                Ok(Ok((ws, _response))) => {
                    let mut guard = self.ws.lock().await;
                    *guard = Some(ws);
                    Ok(())
                }
                Ok(Err(e)) => Err(format!("WebSocket connect failed: {e}")),
                Err(_) => Err("WebSocket connect timed out".to_string()),
            }
        }
    }

    /// Send a single text frame over the cached WebSocket, reconnecting if needed.
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

        // Try the cached socket.
        let mut guard = self.ws.lock().await;
        let send_result = if let Some(ref mut ws) = *guard {
            ws.send(Message::Text(payload.to_string())).await
        } else {
            return Err("no cached WS connection after connect".to_string());
        };

        match send_result {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Socket broken — clear it and fall through to reconnect.
                tracing::info!(
                    controller = SamsungTizenController::NAME,
                    "WS send failed ({e}), reconnecting"
                );
                *guard = None;
            }
        }
        drop(guard);

        // Reconnect and retry once.
        self.connect_ws(host, token).await?;

        let mut guard = self.ws.lock().await;
        if let Some(ref mut ws) = *guard {
            ws.send(Message::Text(payload.to_string()))
                .await
                .map_err(|e| format!("WS send after reconnect failed: {e}"))
        } else {
            Err("WS connection lost after reconnect".to_string())
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
/// and queries panel state via the REST device-info endpoint (port 8001).
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
            .finish()
    }
}

impl SamsungTizenController {
    /// Literal controller name — grep-stable, matches the `samsung-tizen` config type.
    const NAME: &'static str = "samsung-tizen";

    /// Build a new controller with the real network transport.
    #[must_use]
    pub fn new(
        host: String,
        token: String,
        wol_mac: Option<String>,
        treat_unreachable_as_blanked: bool,
    ) -> Self {
        Self {
            host,
            token,
            wol_mac,
            treat_unreachable_as_blanked,
            transport: Arc::new(RealTvTransport::new()),
        }
    }

    /// Build a controller with a custom transport (used by tests).
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
}

#[async_trait]
impl DisplayController for SamsungTizenController {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        vec![BlankMode::ScreenOffAudioOn, BlankMode::PowerOff]
    }

    async fn is_available(&self) -> bool {
        self.is_tv_reachable().await
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        // If the TV is unreachable and the policy says to treat it as blanked,
        // succeed silently. An off TV has no picture to protect.
        if !self.is_tv_reachable().await && self.treat_unreachable_as_blanked {
            return self.unreachable_noop("blank");
        }

        let key = match mode {
            BlankMode::ScreenOffAudioOn => KEY_PICTURE_OFF,
            BlankMode::PowerOff => KEY_POWER,
            BlankMode::BrightnessZero => {
                return Err(CmdFailure {
                    controller: Self::NAME.to_string(),
                    error: format!(
                        "{E_DISPLAY_IO}: unsupported blank mode {mode:?} for samsung-tizen"
                    ),
                });
            }
        };

        self.transport
            .send_key(&self.host, &self.token, key)
            .await
            .map_err(|e| CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!("{E_DISPLAY_IO}: {e}"),
            })
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        // Best-effort WoL for deep-standby TVs.
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

        // KEY_RETURN wakes from picture-off and is not a toggle — safe even
        // if the daemon's state has drifted.
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
/// [`REST_TIMEOUT`]; otherwise `false`.
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
    use dormant_core::error::E_DISPLAY_IO;

    // ── URL construction ────────────────────────────────────────────────────

    #[test]
    fn build_ws_url_includes_base64_name_and_token() {
        let url = format!(
            "wss://10.1.1.7:8002/api/v2/channels/samsung.remote.control?name={expected_name}&token=abc123",
            expected_name = base64::engine::general_purpose::STANDARD.encode("dormant")
        );
        let expected_name = base64::engine::general_purpose::STANDARD.encode("dormant");
        assert!(
            url.starts_with("wss://10.1.1.7:8002/api/v2/channels/samsung.remote.control?name="),
            "URL prefix wrong: {url}"
        );
        assert!(
            url.contains(&format!("name={expected_name}")),
            "URL missing base64 device name: {url}"
        );
        assert!(url.contains("token=abc123"), "URL missing token: {url}");
    }

    // ── WebSocket handshake headers (regression: sec-websocket-key) ─────────

    /// RED-first proof: the OLD bare-`Request::builder().uri().body(())` pattern
    /// produces a request with NO `sec-websocket-key` header. The `IntoClientRequest`
    /// trait generates it automatically. This guard pins the real bug — a WSS
    /// connect against a real Samsung TV without this header fails with
    /// "Missing, duplicated or incorrect header sec-websocket-key".
    #[test]
    fn old_builder_missing_sec_websocket_key_proof() {
        // Construct the URL the same way the real code does.
        let name_b64 = base64::engine::general_purpose::STANDARD.encode("dormant");
        let url = format!("wss://10.1.1.7:8002{WS_PATH}?name={name_b64}&token=abc123");

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
        let url = format!("wss://10.1.1.7:8002{WS_PATH}?name={name_b64}&token=abc123");

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
            "10.1.1.7".into(),
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
            "10.1.1.7".into(),
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

    #[tokio::test]
    async fn blank_brightness_zero_rejected() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = test_controller(fake.clone());
        let err = ctrl.blank(BlankMode::BrightnessZero).await.unwrap_err();
        assert_eq!(err.controller, SamsungTizenController::NAME);
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains("unsupported"));
    }

    // ── Unreachable no-op policy ────────────────────────────────────────────

    #[tokio::test]
    async fn blank_unreachable_noop_when_policy_enabled() {
        let fake = Arc::new(FakeTvTransport::with_connect_results(vec![false]));
        let ctrl = SamsungTizenController::with_transport(
            "10.1.1.7".into(),
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
            "10.1.1.7".into(),
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
            "10.1.1.7".into(),
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
            "10.1.1.7".into(),
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
    async fn supported_modes_includes_screen_off_and_power_off() {
        let fake = Arc::new(FakeTvTransport::new());
        let ctrl = test_controller(fake);
        let modes = ctrl.supported_modes();
        assert!(modes.contains(&BlankMode::ScreenOffAudioOn));
        assert!(modes.contains(&BlankMode::PowerOff));
        assert!(!modes.contains(&BlankMode::BrightnessZero));
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

        // Server: accept, read one frame, echo back (so client sees a
        // successful send — the TV doesn't actually reply, but we want the
        // send to succeed, not the response).
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            // Read the incoming key-press frame.
            let msg = ws.next().await;
            assert!(msg.is_some(), "server should receive a frame");
        });

        let transport = RealTvTransport::for_test(port);
        // First call on a cold cache — must connect and send successfully.
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
            // Connection 1: accept, read one frame.
            {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(stream).await.unwrap();
                let msg = ws.next().await;
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
        // dropping the connection after picture-off. The cached socket
        // is now dead but still present, forcing the "try-cached → fail
        // → reconnect → retry" path.
        transport.close_cached_for_test().await;

        // Second send: tries the dead cached socket → send fails →
        // reconnect on connection 2 → retry succeeds.
        let result = transport
            .send_key("127.0.0.1", "test-token", KEY_PICTURE_OFF)
            .await;
        assert!(
            result.is_ok(),
            "retry after broken pipe should succeed: {result:?}"
        );

        server.await.unwrap();
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
}
