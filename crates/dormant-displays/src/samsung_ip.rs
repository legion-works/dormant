//! Samsung IP Control G2 (port 1516) — JSON-RPC transport for TV
//! settings methods.
//!
//! ## What lives here
//!
//! A thin HTTP/JSON-RPC client for the secondary Samsung control endpoint
//! used as the audio-safe blank path for `samsung-tizen` displays. The
//! primary remote-control path (WebSocket port 8002 with `KEY_PICTURE_OFF`)
//! lives in [`crate::samsung_tizen`] and is unchanged.
//!
//! Port 1516 is **separate from port 8002**:
//!
//! - 8002 is a persistent WebSocket carrying `KEY_*` remote-control events.
//! - 1516 is an HTTPS JSON-RPC endpoint with discrete POSTs. It is used by
//!   Samsung's "Smart View" mobile app for read/write of TV settings.
//!
//! ## Why a second endpoint
//!
//! `KEY_PICTURE_OFF` blanks the panel while audio continues, but it cuts
//! the HDMI source and pauses media. The `backlightControl` JSON-RPC method
//! on port 1516 lets the daemon set the panel backlight 0–50 (0 ≈ near-black
//! dim) without disturbing the source or audio — useful when the operator
//! wants audio to keep playing through the TV speakers but the panel off.
//!
//! ## TLS
//!
//! Like the WebSocket port, the TV presents a self-signed certificate (CN
//! "Samsung IP Control G2"). `reqwest` is configured with
//! `danger_accept_invalid_certs(true)` — the channel is on the local LAN
//! and an attacker who can MITM your LAN already controls the TV. The
//! access-token authentication (below) is the security boundary.
//!
//! ## Auth
//!
//! 1. POST `{"jsonrpc":"2.0","method":"createAccessToken","id":N}` (no
//!    `params` key on the wire — the TV rejects `params: {}` with HTTP 400)
//!    → response includes `"result.AccessToken"`. This unit auto-grants on
//!    the LAN without an on-screen prompt.
//! 2. Every subsequent call includes
//!    `"params":{"AccessToken":"<tok>", ...}`.
//!
//! The token is cached in-memory keyed by host and **persisted to a 0600
//! state file** so a known-good token survives daemon restarts (the TV
//! intermittently fails to re-grant a fresh token on a subsequent
//! `createAccessToken`, so re-acquisition is not always reliable). A
//! `-32010` unauthorized response drops both the in-memory and the
//! persisted entry so the next call re-acquires and writes a fresh token.
//!
//! ## Methods
//!
//! - `backlightControl` (no `backlight` field) → reads `"result.backlight"`
//!   (0–50). Backlight is read via this method, not `getVideoStates` —
//!   `getVideoStates` does not include the backlight field on this TV.
//! - `backlightControl` (with `backlight`) → writes the panel backlight.
//! - `inputSourceControl` → reads `"result.inputSource"`.
//!
//! Samsung reads use `<noun>Control` naming — `get<Noun>` probes return
//! `-32601 Method not found`.
//!
//! Errors are JSON-RPC `{"error":{"code":C,"message":M}}`. Known codes:
//!
//! | code | meaning |
//! |---|---|
//! | -32601 | method not found |
//! | -32001 | not supported |
//! | -32002 | failed / locked |
//! | -32010 | unauthorized |

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[cfg(test)]
use std::sync::Arc;

use async_trait::async_trait;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::types::CmdFailure;
use serde_json::Value;
use serde_json::json;

/// HTTPS port for Samsung IP Control G2.
const IP_CONTROL_PORT: u16 = 1516;

/// URL path on the IP Control endpoint. The TV responds to a POST at the
/// root with a JSON-RPC body — the body determines the method.
const IP_CONTROL_PATH: &str = "/";

/// Request timeout for IP Control calls. Short by design — the endpoint is
/// on the local LAN and a hung call should not wedge the executor.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Log event literal for IP Control token re-acquisition after -32010.
const TOKEN_REACQUIRED: &str = "samsung_ip_token_reacquired";

/// Log event literal for the daemon-owned token state file (load + write).
/// Distinct from `TOKEN_REACQUIRED` so a reader can tell the two apart.
const TOKEN_STATE_LOADED: &str = "samsung_ip_token_state_loaded";
const TOKEN_STATE_WRITTEN: &str = "samsung_ip_token_state_written";

/// The PID separates writers in separate processes; the counter separates
/// concurrent writes from the same process.
static TOKEN_STATE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
type TokenStateHook = Arc<dyn Fn(&Path) + Send + Sync>;

#[cfg(test)]
static TOKEN_STATE_BEFORE_CACHE_HOOK: StdMutex<Option<TokenStateHook>> = StdMutex::new(None);
#[cfg(test)]
static TOKEN_STATE_AFTER_LOAD_HOOK: StdMutex<Option<TokenStateHook>> = StdMutex::new(None);
#[cfg(test)]
static TOKEN_STATE_BEFORE_TEMP_CREATE_HOOK: StdMutex<Option<TokenStateHook>> = StdMutex::new(None);
#[cfg(test)]
static TOKEN_STATE_AFTER_TEMP_CREATE_HOOK: StdMutex<Option<TokenStateHook>> = StdMutex::new(None);
#[cfg(test)]
static TOKEN_STATE_BEFORE_RENAME_HOOK: StdMutex<Option<TokenStateHook>> = StdMutex::new(None);
#[cfg(test)]
static TOKEN_STATE_TEST_HOOK_LOCK: StdMutex<()> = StdMutex::new(());

#[cfg(test)]
type TokenStateWriteFailureHook = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

#[cfg(test)]
static TOKEN_STATE_BEFORE_WRITE_FAILURE_HOOK: StdMutex<Option<TokenStateWriteFailureHook>> =
    StdMutex::new(None);

#[cfg(test)]
fn run_token_state_hook(hook: &StdMutex<Option<TokenStateHook>>, path: &Path) {
    let callback = hook.lock().expect("token-state test hook poisoned").clone();
    if let Some(hook) = callback {
        hook(path);
    }
}

#[cfg(test)]
fn run_token_state_write_failure_hook(path: &Path) -> std::io::Result<()> {
    let callback = TOKEN_STATE_BEFORE_WRITE_FAILURE_HOOK
        .lock()
        .expect("token-state write-failure hook poisoned")
        .clone();
    callback.map_or(Ok(()), |hook| hook(path))
}

// ── JSON-RPC error codes (string anchors — repo grep rule) ──────────────────────

/// JSON-RPC method-not-found.
pub const E_JSONRPC_METHOD_NOT_FOUND: &str = "-32601";
/// JSON-RPC not-supported.
pub const E_JSONRPC_NOT_SUPPORTED: &str = "-32001";
/// JSON-RPC failed/locked.
pub const E_JSONRPC_FAILED_OR_LOCKED: &str = "-32002";
/// JSON-RPC unauthorized.
pub const E_JSONRPC_UNAUTHORIZED: &str = "-32010";

// ── BacklightTransport trait — network boundary for test injection ─────────────

/// Abstract transport for Samsung IP Control G2 (port 1516).
///
/// The real implementation talks HTTPS to the TV with self-signed certs
/// accepted and a host-keyed token cache. The fake used in tests records
/// calls and returns pre-programmed responses.
#[async_trait]
pub trait BacklightTransport: Send + Sync {
    /// Acquire an access token for `host` (cached after first call).
    ///
    /// Returns `Ok(token)` on success. On JSON-RPC `-32010` mid-session,
    /// the transport drops the cached token and re-acquires on the next
    /// call.
    async fn acquire_token(&self, host: &str) -> Result<String, String>;

    /// Read the current panel backlight (0–50).
    async fn get_backlight(&self, host: &str, token: &str) -> Result<u8, String>;

    /// Read the active input source (for example, `HDMI4`).
    async fn input_source(&self, host: &str, token: &str) -> Result<String, String>;

    /// Set the panel backlight (0–50; 0 ≈ dim).
    async fn set_backlight(&self, host: &str, token: &str, value: u8) -> Result<(), String>;

    /// Drop any cached token for `host`. Called by the controller after a
    /// `-32010` unauthorized response so the next `acquire_token` returns
    /// a fresh one. The default implementation is a no-op; the real
    /// transport's override reaches into its internal cache.
    fn invalidate_token(&self, _host: &str) {}
}

// ── Real transport ──────────────────────────────────────────────────────────────

/// Production transport: reqwest + rustls + `danger_accept_invalid_certs`.
///
/// The transport is shared by all `BacklightControl` calls against any host
/// it has been asked about. Tokens are cached in-process, keyed by host.
///
/// `base_url` lets tests point the transport at a wiremock (or any other
/// URL scheme) without exercising the LAN-HTTPS path. Production sets it
/// to `None`, which falls through to the hardcoded
/// `https://{host}:{IP_CONTROL_PORT}/` pattern.
pub struct RealBacklightTransport {
    client: reqwest::Client,
    token_cache: StdMutex<HashMap<String, String>>,
    /// `Some` for tests that point the transport at a mock URL; `None`
    /// (the production default) uses the LAN-HTTPS pattern.
    base_url: Option<String>,
    /// Production path to the daemon-owned token state file
    /// (`$XDG_STATE_HOME/dormant/samsung-ip-tokens.json` or fallback).
    /// `None` disables persistence — used by `for_test_with_base_url`
    /// (no real state file in unit/wiremock tests) and by `for_test_with_state_path`
    /// (which points at a caller-supplied temp path).
    state_path: Option<PathBuf>,
    #[cfg(test)]
    skip_state_file_lock: bool,
}

impl RealBacklightTransport {
    /// Build a new transport with the default 5-second request timeout
    /// and persistence to the daemon-owned state file
    /// (`$XDG_STATE_HOME/dormant/samsung-ip-tokens.json` or
    /// `~/.local/state/dormant/samsung-ip-tokens.json`). On construction
    /// the state file is loaded (if present) into the in-memory cache so
    /// a known-good token survives restarts.
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::with_timeout(REQUEST_TIMEOUT)
    }

    /// Build a transport with a custom timeout (used by tests). Production
    /// wires the default state-file path so the persisted token is
    /// available on startup.
    ///
    /// # Panics
    ///
    /// Panics if the `reqwest::Client` builder fails — this only happens
    /// for invalid TLS configuration, which `timeout` and
    /// `danger_accept_invalid_certs` cannot trigger.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        let state_path = default_state_path();
        let transport = Self::with_timeout_state_path(timeout, state_path);
        // Seed the in-memory cache from the on-disk file so a restart
        // with a known-good token does NOT re-acquire.
        if let Some(path) = transport.state_path.as_ref() {
            let loaded = load_token_state(path);
            if let Ok(map) = loaded {
                if !map.is_empty() {
                    tracing::info!(
                        event = TOKEN_STATE_LOADED,
                        count = map.len(),
                        path = %path.display(),
                        "samsung-ip: loaded persisted tokens from state file",
                    );
                }
                *transport.token_cache.lock().expect("token cache poisoned") = map;
            }
            // A read failure on the state file is non-fatal — fall through
            // to fresh acquisition. The TV will reject stale tokens with
            // -32010 and the transport will re-acquire.
        }
        transport
    }

    /// Internal: build the transport with an explicit state-file path.
    fn with_timeout_state_path(timeout: Duration, state_path: Option<PathBuf>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(true)
            // The TV's HTTP parser 400s on lowercase header names;
            // reqwest lowercases by default — force title-case on the wire.
            .http1_title_case_headers()
            .build()
            .expect("reqwest::Client::builder should never fail with default settings");
        Self {
            client,
            token_cache: StdMutex::new(HashMap::new()),
            base_url: None,
            state_path,
            #[cfg(test)]
            skip_state_file_lock: false,
        }
    }

    /// Build a transport whose URLs are rooted at `base_url` instead of the
    /// LAN-HTTPS pattern — used by tests that stand up a wiremock (or any
    /// other server) and want to drive `RealBacklightTransport` through the
    /// full `reqwest` round-trip (request shape, JSON-RPC envelope, error
    /// mapping, token cache, `-32010` re-acquire). No state file is
    /// loaded or written; the transport is hermetic.
    ///
    /// # Panics
    ///
    /// Panics if the `reqwest::Client` builder fails for any reason.
    #[cfg(test)]
    #[must_use]
    pub fn for_test_with_base_url(base_url: String, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest::Client::builder should never fail with default settings");
        Self {
            client,
            token_cache: StdMutex::new(HashMap::new()),
            base_url: Some(base_url),
            state_path: None,
            skip_state_file_lock: false,
        }
    }

    /// Build a transport whose URLs are rooted at `base_url` and whose
    /// persisted tokens live at `state_path` — used by tests that need
    /// to exercise the persistence + invalidation plumbing without
    /// touching the real per-user state directory.
    ///
    /// # Panics
    ///
    /// Panics if the `reqwest::Client` builder fails for any reason.
    #[cfg(test)]
    #[must_use]
    pub fn for_test_with_state_path(
        base_url: String,
        timeout: Duration,
        state_path: PathBuf,
    ) -> Self {
        let mut transport = Self::for_test_with_base_url(base_url, timeout);
        // Seed the in-memory cache from the on-disk file (if present).
        let loaded = load_token_state(&state_path);
        if let Ok(map) = loaded {
            *transport.token_cache.lock().expect("token cache poisoned") = map;
        }
        transport.state_path = Some(state_path);
        transport
    }

    /// Build a state-file transport that bypasses the cross-process lock so
    /// unit tests can isolate the in-process cache-lock invariant.
    #[cfg(test)]
    fn for_test_with_state_path_without_file_lock(
        base_url: String,
        timeout: Duration,
        state_path: PathBuf,
    ) -> Self {
        let mut transport = Self::for_test_with_state_path(base_url, timeout, state_path);
        transport.skip_state_file_lock = true;
        transport
    }

    /// Send a JSON-RPC POST and parse the response.
    ///
    /// The `params` argument is `Option<Value>` — `None` omits the `params`
    /// key from the JSON-RPC envelope entirely. The Samsung TV rejects
    /// `params: {}` with HTTP 400 on `createAccessToken`, so omitting the
    /// key is the only correct shape for that method. Every other method
    /// passes `Some(...)`.
    ///
    /// On a JSON-RPC `error` field, the returned `Err` carries the literal
    /// `code` plus a short description so the controller can map it to a
    /// `CmdFailure` with the right prefix.
    async fn call(&self, host: &str, method: &str, params: Option<Value>) -> Result<Value, String> {
        let url = match &self.base_url {
            Some(base) => format!("{base}/"),
            None => format!("https://{host}:{IP_CONTROL_PORT}{IP_CONTROL_PATH}"),
        };
        let mut body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "id": 1,
        });
        if let Some(p) = params {
            body["params"] = p;
        }
        let resp = self
            .client
            .post(&url)
            // The port-1516 endpoint is pedantic: a request with the
            // reqwest default `Accept: */*` returns HTTP 400 Bad Request.
            // Pin to `application/json` so every IP Control method matches.
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| format!("response parse failed: {e}"))?;
        if let Some(err) = v.get("error") {
            let code = err
                .get("code")
                .and_then(Value::as_i64)
                .map_or_else(|| "unknown".to_string(), |n| n.to_string());
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            return Err(format!("{code} {message}").trim().to_string());
        }
        Ok(v)
    }

    fn persist_token(&self, host: &str, token: &str) -> Result<(), String> {
        #[cfg(test)]
        run_token_state_hook(
            &TOKEN_STATE_BEFORE_CACHE_HOOK,
            self.state_path.as_deref().unwrap_or_else(|| Path::new(".")),
        );

        let mut cache = self.token_cache.lock().expect("token cache poisoned");
        cache.insert(host.to_string(), token.to_string());

        let result = match self.state_path.as_ref() {
            Some(path) => self.update_token_state(path, |map| {
                map.insert(host.to_string(), token.to_string());
            }),
            None => Ok(()),
        };
        drop(cache);
        result
    }

    fn update_token_state(
        &self,
        path: &Path,
        update: impl FnOnce(&mut HashMap<String, String>),
    ) -> Result<(), String> {
        #[cfg(not(test))]
        let _ = self;
        let write = || update_token_state_unlocked(path, update);
        #[cfg(test)]
        if self.skip_state_file_lock {
            return write();
        }
        with_token_state_file_lock(path, write)
    }

    /// Drop the cached token for `host` (called on `-32010`). Also removes
    /// the entry from the on-disk state file so a daemon restart does not
    /// re-load a known-stale token. A failure to update the state file is
    /// logged at WARN but does not propagate — the in-memory invalidation
    /// is what unblocks the next re-acquire.
    fn invalidate_token_inner(&self, host: &str) {
        let mut cache = self.token_cache.lock().ok();
        if let Some(cache) = cache.as_mut() {
            cache.remove(host);
        }
        if let Some(path) = self.state_path.as_ref() {
            let mut removed = false;
            if let Err(e) = self.update_token_state(path, |map| {
                removed = map.remove(host).is_some();
            }) && removed
            {
                tracing::warn!(
                    event = TOKEN_STATE_WRITTEN,
                    path = %path.display(),
                    error = %e,
                    "samsung-ip: failed to update state file on invalidate",
                );
            }
        }
        drop(cache);
    }

    /// Drop the cached token for `host` (called on `-32010`).
    #[cfg(test)]
    pub fn invalidate_token_for_test(&self, host: &str) {
        self.invalidate_token(host);
    }
}

impl Default for RealBacklightTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BacklightTransport for RealBacklightTransport {
    async fn acquire_token(&self, host: &str) -> Result<String, String> {
        if let Some(tok) = self
            .token_cache
            .lock()
            .expect("token cache poisoned")
            .get(host)
            .cloned()
        {
            return Ok(tok);
        }

        // Proven wire shape: the `params` key MUST be absent (the TV
        // rejects `params: {}` with HTTP 400).
        let response = self.call(host, "createAccessToken", None).await?;

        let token = response
            .get("result")
            .and_then(|r| r.get("AccessToken"))
            .and_then(Value::as_str)
            .ok_or_else(|| "token parse failed: missing result.AccessToken".to_string())?
            .to_string();

        // Persist immediately so a daemon restart reuses this token
        // instead of triggering an on-screen allow prompt.
        if let Err(e) = self.persist_token(host, &token) {
            let path = self.state_path.as_deref().unwrap_or_else(|| Path::new("."));
            tracing::warn!(
                event = TOKEN_STATE_WRITTEN,
                path = %path.display(),
                error = %e,
                "samsung-ip: failed to persist token to state file",
            );
        }
        Ok(token)
    }

    async fn get_backlight(&self, host: &str, token: &str) -> Result<u8, String> {
        // Proven wire shape: backlightControl with ONLY AccessToken and
        // NO `backlight` field — the TV returns `result.backlight`. The
        // previous `getVideoStates` call always failed because that
        // method does not carry a backlight field on this TV.
        let value = self
            .call(
                host,
                "backlightControl",
                Some(json!({ "AccessToken": token })),
            )
            .await?;
        let backlight = value
            .get("result")
            .and_then(|r| r.get("backlight"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "missing result.backlight".to_string())?;
        u8::try_from(backlight).map_err(|e| format!("backlight out of range: {e}"))
    }

    async fn input_source(&self, host: &str, token: &str) -> Result<String, String> {
        let value = self
            .call(
                host,
                "inputSourceControl",
                Some(json!({ "AccessToken": token })),
            )
            .await?;
        value
            .get("result")
            .and_then(|result| result.get("inputSource"))
            .and_then(Value::as_str)
            .filter(|source| !source.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| "missing result.inputSource".to_owned())
    }

    async fn set_backlight(&self, host: &str, token: &str, value: u8) -> Result<(), String> {
        self.call(
            host,
            "backlightControl",
            Some(json!({ "AccessToken": token, "backlight": value })),
        )
        .await?;
        Ok(())
    }

    fn invalidate_token(&self, host: &str) {
        self.invalidate_token_inner(host);
    }
}

// ── State-file plumbing ─────────────────────────────────────────────────────────

/// Default location of the daemon-owned token state file.
///
/// 1. `$XDG_STATE_HOME/dormant/samsung-ip-tokens.json`
/// 2. `~/.local/state/dormant/samsung-ip-tokens.json`
///
/// Directory precedence is owned by
/// [`dormant_core::paths::state_dir_from_env`] — this function only
/// decides whether persistence is possible at all (`None` only when
/// NEITHER `XDG_STATE_HOME` nor `HOME` is set, which is exceedingly rare
/// in practice: the daemon would still start, only the in-memory cache
/// would be used) and appends the token file name. Kept private to
/// `samsung_ip` because it is daemon-internal state — distinct from
/// `credentials.toml`, which the user owns.
fn default_state_path() -> Option<PathBuf> {
    state_path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// Internal: derive the token state path from explicit env values (test
/// seam). `None` only when BOTH `xdg` and `home` are absent — that is the
/// only "persistence disabled" case this function decides; everything
/// else (XDG-state vs. `HOME` fallback precedence) is delegated to
/// `dormant_core::paths::state_dir_from_env` so precedence logic has a
/// single source of truth.
fn state_path_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if xdg.is_none() && home.is_none() {
        return None;
    }
    Some(dormant_core::paths::state_dir_from_env(xdg, home).join("samsung-ip-tokens.json"))
}

/// Load the persisted token map from `path`. Returns an empty map when
/// the file does not exist. Parse errors are surfaced so a malformed
/// state file is not silently ignored — the operator should notice.
fn load_token_state(path: &Path) -> Result<HashMap<String, String>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read samsung-ip token state '{}': {e}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    serde_json::from_str::<HashMap<String, String>>(&raw)
        .map_err(|e| format!("parse samsung-ip token state '{}': {e}", path.display()))
}

fn update_token_state_unlocked(
    path: &Path,
    update: impl FnOnce(&mut HashMap<String, String>),
) -> Result<(), String> {
    let mut map = load_token_state(path).unwrap_or_default();
    #[cfg(test)]
    run_token_state_hook(&TOKEN_STATE_AFTER_LOAD_HOOK, path);
    update(&mut map);
    write_token_state(path, &map)
}

fn with_token_state_file_lock<T>(
    path: &Path,
    write: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    #[cfg(unix)]
    {
        use rustix::fs::{FlockOperation, flock};
        ensure_token_state_parent(path)?;
        let lock_path = token_state_lock_path(path);
        let lock_file = {
            use std::os::unix::fs::OpenOptionsExt as _;

            std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .mode(0o600)
                .open(&lock_path)
                .map_err(|e| {
                    format!(
                        "open samsung-ip token state lock '{}': {e}",
                        lock_path.display()
                    )
                })?
        };
        flock(&lock_file, FlockOperation::LockExclusive)
            .map_err(|e| format!("lock samsung-ip token state '{}': {e}", lock_path.display()))?;
        write()
    }
    #[cfg(not(unix))]
    {
        let _ = (path, write);
        // Persisting without an exercised advisory lock would silently revive
        // the token-loss race; Windows remains disabled until it is tested.
        Err("samsung-ip token state persistence requires a supported advisory file lock on this platform".to_string())
    }
}

fn ensure_token_state_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create samsung-ip token state dir '{}': {e}",
                parent.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Best-effort tighten: create_dir_all respects the umask, so
            // explicitly set 0o700 once the dir exists. A failure here is
            // not fatal — `credentials.toml` follows the same pattern.
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    Ok(())
}

fn token_state_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("tokens");
    path.with_file_name(format!(".{file_name}.lock"))
}

fn token_state_tmp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("tokens");
    let seq = TOKEN_STATE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{file_name}.tmp.{}.{seq}", std::process::id()))
}

fn remove_token_state_tmp(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "remove samsung-ip token state tmp '{}': {e}",
            path.display()
        )),
    }
}

/// Atomically write the token map to `path`. The write is atomic (temp
/// file in the same directory + rename) so a crash mid-write never
/// corrupts an existing good state file. On Unix the file is created
/// mode `0o600` (owner read/write only) and the directory `0o700`
/// (owner only) — same boundary as `credentials.toml`.
///
/// The mode is set by `OpenOptions` at creation rather than applied
/// afterwards: a `create` followed by a `set_permissions` leaves the
/// file briefly readable under a permissive umask, and the token bytes
/// are already on disk by then.
///
/// Not reachable on non-Unix: [`with_token_state_file_lock`] refuses to
/// run its write closure at all without a supported advisory file lock,
/// so the `cfg(not(unix))` arm below exists only to keep portability
/// builds compiling.
fn write_token_state(path: &Path, map: &HashMap<String, String>) -> Result<(), String> {
    use std::io::Write as _;

    ensure_token_state_parent(path)?;

    let raw = serde_json::to_string_pretty(map)
        .map_err(|e| format!("serialize samsung-ip token state: {e}"))?;

    let tmp = token_state_tmp_path(path);
    #[cfg(test)]
    run_token_state_hook(&TOKEN_STATE_BEFORE_TEMP_CREATE_HOOK, &tmp);
    let mut temp_created = false;
    let write_result = (|| {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

            let f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| {
                    format!("create samsung-ip token state tmp '{}': {e}", tmp.display())
                })?;
            temp_created = true;
            #[cfg(test)]
            run_token_state_hook(&TOKEN_STATE_AFTER_TEMP_CREATE_HOOK, &tmp);
            let mode = f
                .metadata()
                .map_err(|e| {
                    format!(
                        "inspect samsung-ip token state tmp '{}': {e}",
                        tmp.display()
                    )
                })?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o600 {
                return Err(format!(
                    "create samsung-ip token state tmp '{}' with mode 0o600: got {mode:o}",
                    tmp.display()
                ));
            }
            f
        };
        #[cfg(not(unix))]
        let mut f = {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| {
                    format!("create samsung-ip token state tmp '{}': {e}", tmp.display())
                })?;
            temp_created = true;
            #[cfg(test)]
            run_token_state_hook(&TOKEN_STATE_AFTER_TEMP_CREATE_HOOK, &tmp);
            f
        };
        #[cfg(test)]
        run_token_state_write_failure_hook(&tmp)
            .map_err(|e| format!("write samsung-ip token state tmp: {e}"))?;
        f.write_all(raw.as_bytes())
            .map_err(|e| format!("write samsung-ip token state tmp: {e}"))?;
        f.sync_all()
            .map_err(|e| format!("fsync samsung-ip token state tmp: {e}"))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        if !temp_created {
            return Err(e);
        }
        return match remove_token_state_tmp(&tmp) {
            Ok(()) => Err(e),
            Err(remove_err) => Err(format!("{e}; {remove_err}")),
        };
    }
    #[cfg(test)]
    run_token_state_hook(&TOKEN_STATE_BEFORE_RENAME_HOOK, path);
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let rename_err = format!(
                "rename samsung-ip token state '{}' -> '{}': {e}",
                tmp.display(),
                path.display()
            );
            match remove_token_state_tmp(&tmp) {
                Ok(()) => Err(rename_err),
                Err(remove_err) => Err(format!("{rename_err}; {remove_err}")),
            }
        }
    }
}

// ── Fake transport for tests ───────────────────────────────────────────────────

/// Test-only transport that records calls and returns pre-programmed responses.
///
/// Constructed via [`FakeBacklightTransport::new`]; populate the queues with
/// the desired return sequence before exercising the controller.
#[derive(Debug, Default)]
pub struct FakeBacklightTransport {
    /// Return values for successive `acquire_token` calls.
    pub acquire_results: StdMutex<Vec<Result<String, String>>>,
    /// Return values for successive `get_backlight` calls.
    pub get_results: StdMutex<Vec<Result<u8, String>>>,
    /// Return values for successive `input_source` calls.
    pub input_source_results: StdMutex<Vec<Result<String, String>>>,
    /// Return values for successive `set_backlight` calls.
    pub set_results: StdMutex<Vec<Result<(), String>>>,
    /// Hosts that requested `acquire_token`, in order.
    pub acquire_hosts: StdMutex<Vec<String>>,
    /// `(host, value)` tuples passed to `set_backlight`, in order.
    pub set_calls: StdMutex<Vec<(String, u8)>>,
    /// Hosts + tokens passed to `get_backlight`, in order.
    pub get_calls: StdMutex<Vec<(String, String)>>,
    /// Hosts + tokens passed to `input_source`, in order.
    pub input_source_calls: StdMutex<Vec<(String, String)>>,
}

impl FakeBacklightTransport {
    /// Build a new empty fake — all queues default to "return the
    /// documented default value when nothing has been programmed".
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BacklightTransport for FakeBacklightTransport {
    async fn acquire_token(&self, host: &str) -> Result<String, String> {
        self.acquire_hosts.lock().unwrap().push(host.to_string());
        let mut results = self.acquire_results.lock().unwrap();
        if results.is_empty() {
            Ok("fake-token".to_string())
        } else {
            results.remove(0)
        }
    }

    async fn get_backlight(&self, host: &str, token: &str) -> Result<u8, String> {
        self.get_calls
            .lock()
            .unwrap()
            .push((host.to_string(), token.to_string()));
        let mut results = self.get_results.lock().unwrap();
        if results.is_empty() {
            Ok(40)
        } else {
            results.remove(0)
        }
    }

    async fn input_source(&self, host: &str, token: &str) -> Result<String, String> {
        self.input_source_calls
            .lock()
            .unwrap()
            .push((host.to_string(), token.to_string()));
        let mut results = self.input_source_results.lock().unwrap();
        if results.is_empty() {
            Ok("HDMI1".to_string())
        } else {
            results.remove(0)
        }
    }

    async fn set_backlight(&self, host: &str, _token: &str, value: u8) -> Result<(), String> {
        self.set_calls
            .lock()
            .unwrap()
            .push((host.to_string(), value));
        let mut results = self.set_results.lock().unwrap();
        if results.is_empty() {
            Ok(())
        } else {
            results.remove(0)
        }
    }

    fn invalidate_token(&self, host: &str) {
        self.acquire_hosts
            .lock()
            .unwrap()
            .push(format!("invalidate:{host}"));
    }
}

// ── Helper: classify JSON-RPC error codes ───────────────────────────────────────

/// Return the canonical JSON-RPC code literal for a transport error string.
///
/// Strips leading `-` so callers can pass either `"-32010"` or `"32010"`.
/// Tolerates trailing text (e.g. `"-32010 unauthorized"`) by extracting
/// just the leading digit run. Unknown codes fall back to a generic
/// anchor (`jsonrpc_error`) so the dispatcher still sees a stable grep
/// anchor.
#[must_use]
pub fn classify_jsonrpc_error(raw: &str) -> &'static str {
    let trimmed = raw.trim().trim_start_matches('-');
    // Walk leading digits only — transport error strings may have
    // appended human text (e.g. "-32010 unauthorized") after the code.
    let code: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    match code.as_str() {
        "32601" => E_JSONRPC_METHOD_NOT_FOUND,
        "32001" => E_JSONRPC_NOT_SUPPORTED,
        "32002" => E_JSONRPC_FAILED_OR_LOCKED,
        "32010" => E_JSONRPC_UNAUTHORIZED,
        _ => "jsonrpc_error",
    }
}

/// Build a `CmdFailure` from a transport error string.
///
/// On `-32010` unauthorized, the token is dropped from the cache so the
/// controller's retry acquires a fresh one. Other codes pass through with
/// the JSON-RPC code embedded as a grep-stable anchor.
#[must_use]
pub fn map_transport_error(
    controller_name: &str,
    transport: &dyn BacklightTransport,
    host: &str,
    raw_err: &str,
) -> CmdFailure {
    let classified = classify_jsonrpc_error(raw_err);
    if classified == E_JSONRPC_UNAUTHORIZED {
        tracing::info!(
            event = TOKEN_REACQUIRED,
            host,
            "samsung-ip: token rejected (-32010); will re-acquire"
        );
        transport.invalidate_token(host);
    }
    CmdFailure {
        controller: controller_name.to_string(),
        error: format!("{E_DISPLAY_IO}: samsung-ip {classified}: {raw_err}"),
    }
}

// ── AppVisibilityProbe — port 8001 app visibility for the source gate ──────────

/// HTTP port for the Tizen REST applications + device-info API. Distinct
/// from the WebSocket control port (`8002`, used by [`crate::samsung_tizen`])
/// and the JSON-RPC IP Control port (`1516`, used above). The endpoint is
/// unauthenticated on the LAN — no token required.
pub const TIZEN_REST_PORT: u16 = 8001;

/// URL path template for a single installed-app query.
///
/// `GET /api/v2/applications/{app_id}` returns
/// `{"name": "...", "running": bool, "visible": bool}`. `visible: true` is
/// the screen-ownership oracle the active-sampling source gate reads —
/// `inputSourceControl` on port 1516 stays stale while a Tizen app owns
/// the panel (issue #232).
pub const APPLICATIONS_PATH: &str = "/api/v2/applications/";

/// Per-app visibility probe timeout. Kept tight so a configured catalog of
/// 5–8 apps fits inside the 15-second source-poll budget even on a
/// sluggish LAN link.
pub const APP_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Log event literal: one or more watched apps is currently visible.
pub const APP_VISIBLE_OBSERVED: &str = "wear_sampling_app_visible";

/// Log event literal: an app-visibility probe failed (network, parse,
/// non-2xx). Emitted at most once per poll cycle per failing app; the
/// gate fails-safe to the input-only verdict on this signal.
pub const APP_PROBE_FAILED: &str = "wear_sampling_app_probe_failed";

/// Tri-state visibility outcome for a single configured app probe.
///
/// The source gate combines these into its overall verdict:
/// - **any** `Visible` flips the gate to `Mismatched { observed: "app_visible:<id>" }`,
///   regardless of `inputSourceControl` (issue #232 — apps own the panel
///   without changing the reported input source).
/// - `Unknown` only DEGRADES the input-only verdict when `inputSourceControl`
///   also failed; an unknown probe never flips the gate on its own (fail
///   toward uniform attribution, not toward false-positive mismatch).
/// - `NotVisible` is a successful observation that the app is not on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppVisibility {
    /// The app is currently visible (owns the panel).
    Visible,
    /// The app is installed but not visible.
    NotVisible,
    /// The probe could not establish visibility (network/parse/timeout).
    Unknown,
}

impl AppVisibility {
    /// Wire-friendly stable tag for logs and CLI surfaces.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::NotVisible => "not_visible",
            Self::Unknown => "unknown",
        }
    }
}

/// Network boundary for the port-8001 app-visibility probe.
///
/// The real impl talks plain HTTP to the LAN-local TV; the fake used in
/// tests records calls and returns pre-programmed outcomes.
#[async_trait]
pub trait AppVisibilityProbe: Send + Sync {
    /// Probe one installed-app id and report the tri-state outcome.
    async fn probe(&self, host: &str, app_id: &str) -> AppVisibility;
}

/// Production probe: `reqwest` + plain HTTP (port 8001 is unauthenticated,
/// the LAN threat model is identical to the WebSocket/REST paths already
/// used in [`crate::samsung_tizen`]). `APP_PROBE_TIMEOUT` bounds every
/// call so a flaky network cannot pin the gate in `unknown`.
pub struct RealAppVisibilityProbe {
    client: reqwest::Client,
    /// `None` in production (URLs are built from `host:TIZEN_REST_PORT`);
    /// `Some` for tests that root URLs at a wiremock or other base URL.
    base_url: Option<String>,
}

impl RealAppVisibilityProbe {
    /// Build a probe with the documented per-app timeout.
    ///
    /// # Panics
    ///
    /// Panics if the `reqwest::Client` builder fails — does not happen
    /// with the default settings used here.
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(APP_PROBE_TIMEOUT)
            .build()
            .expect("reqwest::Client::builder should never fail with default settings");
        Self {
            client,
            base_url: None,
        }
    }

    /// Build a probe whose URLs are rooted at `base_url` (tests can point
    /// at a wiremock without exercising the LAN path). The
    /// `host` argument to `probe()` is appended after `base_url` so a
    /// wiremock running on `127.0.0.1` is hit without DNS lookups.
    ///
    /// # Panics
    ///
    /// Panics if the `reqwest::Client` builder fails — does not happen
    /// with the default settings used here.
    #[cfg(test)]
    #[must_use]
    pub fn for_test_with_base_url(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(APP_PROBE_TIMEOUT)
            .build()
            .expect("reqwest::Client::builder should never fail");
        Self {
            client,
            base_url: Some(base_url),
        }
    }
}

impl Default for RealAppVisibilityProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AppVisibilityProbe for RealAppVisibilityProbe {
    async fn probe(&self, host: &str, app_id: &str) -> AppVisibility {
        let url = match &self.base_url {
            // Test mode: trust the base_url and ignore the `host`
            // argument (so wiremock's auto-assigned port works without
            // re-resolution). Production never sets base_url.
            Some(base) => format!("{base}{APPLICATIONS_PATH}{app_id}"),
            None => format!("http://{host}:{TIZEN_REST_PORT}{APPLICATIONS_PATH}{app_id}"),
        };
        let Ok(response) = self.client.get(&url).send().await else {
            return AppVisibility::Unknown;
        };
        if !response.status().is_success() {
            // The TV responds with 404 for an id it does not know about.
            // Treat as `not_visible` rather than `unknown` — the TV gave
            // us a definitive answer, just not the one we hoped for. 5xx
            // remains `unknown` (server-side problem, retry next cycle).
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return AppVisibility::NotVisible;
            }
            return AppVisibility::Unknown;
        }
        let body: Value = match response.json().await {
            Ok(b) => b,
            Err(_) => return AppVisibility::Unknown,
        };
        match body.get("visible").and_then(Value::as_bool) {
            Some(true) => AppVisibility::Visible,
            Some(false) => AppVisibility::NotVisible,
            // The body does not include a `visible` field — treat as
            // unknown rather than inferring (older Tizen firmware may
            // omit the field).
            None => AppVisibility::Unknown,
        }
    }
}

// ── Test fake for app visibility ────────────────────────────────────────────────

/// Test-only probe that records calls and returns pre-programmed outcomes.
///
/// The default for an unset app id is `NotVisible` (mirroring the real
/// probe's 404 response) so tests can configure exactly one app and see
/// the desired path without scripting every catalog entry.
#[derive(Debug, Default)]
pub struct FakeAppVisibilityProbe {
    /// Hosts + app ids probed, in order — the poller's per-cycle
    /// sequence is observable from the outside.
    pub calls: StdMutex<Vec<(String, String)>>,
    /// Pre-programmed outcomes keyed by app id. Missing keys yield
    /// `NotVisible`.
    pub results: StdMutex<HashMap<String, AppVisibility>>,
    /// Force the next probe to return `Unknown` for any id (lets tests
    /// simulate a fully unreachable 8001 endpoint with one flag).
    pub force_unknown: StdMutex<bool>,
}

impl FakeAppVisibilityProbe {
    /// Build an empty fake.
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-program an app id's outcome.
    ///
    /// # Panics
    ///
    /// Panics if the results `Mutex` is poisoned — does not happen in
    /// the test-only call patterns this method supports.
    pub fn set(&self, app_id: &str, outcome: AppVisibility) {
        self.results
            .lock()
            .unwrap()
            .insert(app_id.to_string(), outcome);
    }
}

#[async_trait]
impl AppVisibilityProbe for FakeAppVisibilityProbe {
    async fn probe(&self, host: &str, app_id: &str) -> AppVisibility {
        self.calls
            .lock()
            .unwrap()
            .push((host.to_string(), app_id.to_string()));
        if *self.force_unknown.lock().unwrap() {
            return AppVisibility::Unknown;
        }
        self.results
            .lock()
            .unwrap()
            .get(app_id)
            .copied()
            .unwrap_or(AppVisibility::NotVisible)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    #[test]
    fn classify_jsonrpc_method_not_found() {
        assert_eq!(
            classify_jsonrpc_error("-32601 boom"),
            E_JSONRPC_METHOD_NOT_FOUND
        );
    }

    #[test]
    fn classify_jsonrpc_not_supported() {
        assert_eq!(classify_jsonrpc_error("-32001"), E_JSONRPC_NOT_SUPPORTED);
    }

    #[test]
    fn classify_jsonrpc_failed_or_locked() {
        assert_eq!(
            classify_jsonrpc_error("-32002 not now"),
            E_JSONRPC_FAILED_OR_LOCKED
        );
    }

    #[test]
    fn classify_jsonrpc_unauthorized() {
        assert_eq!(
            classify_jsonrpc_error("-32010 token bad"),
            E_JSONRPC_UNAUTHORIZED
        );
    }

    #[test]
    fn classify_jsonrpc_unknown_code() {
        assert_eq!(classify_jsonrpc_error("-99999 mystery"), "jsonrpc_error");
    }

    #[test]
    fn classify_jsonrpc_handles_no_leading_minus() {
        assert_eq!(classify_jsonrpc_error("32601"), E_JSONRPC_METHOD_NOT_FOUND);
    }

    /// `state_path_from` appends the token file name onto whatever state
    /// directory `dormant_core::paths::state_dir_from_env` derives —
    /// directory precedence itself (XDG-state vs. `HOME` fallback) is
    /// `dormant-core`'s responsibility and is covered by that crate's own
    /// tests. This only checks the filename append plus the "no env at
    /// all" persistence-disabled branch.
    #[test]
    fn state_path_from_appends_token_filename() {
        let p = state_path_from(Some(OsString::from("/run/state/dormant")), None)
            .expect("Some(xdg) in, Some(path) out");
        assert_eq!(
            p,
            std::path::PathBuf::from("/run/state/dormant/dormant/samsung-ip-tokens.json")
        );
    }

    #[test]
    fn state_path_from_no_env_returns_none() {
        assert!(state_path_from(None, None).is_none());
    }

    /// `write_token_state` creates the file with mode `0o600` on Unix
    /// (and 0o700 on the parent dir) — same boundary as `credentials.toml`.
    /// A regression to a world-readable mode would expose the access
    /// token to other users on the host.
    #[cfg(unix)]
    #[test]
    fn write_token_state_creates_file_with_mode_0o600() {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir/samsung-ip-tokens.json");
        let mut map = HashMap::new();
        map.insert("192.0.2.7".to_string(), "tok-secret".to_string());
        write_token_state(&path, &map).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "state file must be 0o600 (owner read+write only): got {mode:o}"
        );

        let dir_mode = std::fs::metadata(dir.path().join("subdir"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            dir_mode & 0o777,
            0o700,
            "parent dir must be 0o700: got {dir_mode:o}"
        );
    }

    /// The token temp must be owner-only before its first byte is written;
    /// tightening a wider file afterwards leaves a credential-read window.
    #[cfg(unix)]
    #[test]
    fn token_state_temp_is_mode_0o600_at_creation() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;

        let _serial = TOKEN_STATE_TEST_HOOK_LOCK
            .lock()
            .expect("token-state test hook lock poisoned");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("samsung-ip-tokens.json");
        let (mode_tx, mode_rx) = mpsc::channel();
        *TOKEN_STATE_AFTER_TEMP_CREATE_HOOK
            .lock()
            .expect("token-state after-create hook poisoned") = Some(Arc::new(move |tmp| {
            mode_tx
                .send(std::fs::metadata(tmp).unwrap().permissions().mode() & 0o777)
                .unwrap();
        }));

        write_token_state(&path, &HashMap::new()).unwrap();

        *TOKEN_STATE_AFTER_TEMP_CREATE_HOOK
            .lock()
            .expect("token-state after-create hook poisoned") = None;
        assert_eq!(mode_rx.recv().unwrap(), 0o600);
    }

    #[test]
    fn token_state_refuses_preexisting_temp_file() {
        use std::sync::mpsc;

        let _serial = TOKEN_STATE_TEST_HOOK_LOCK
            .lock()
            .expect("token-state test hook lock poisoned");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("samsung-ip-tokens.json");
        let (tmp_tx, tmp_rx) = mpsc::channel();
        *TOKEN_STATE_BEFORE_TEMP_CREATE_HOOK
            .lock()
            .expect("token-state before-create hook poisoned") = Some(Arc::new(move |tmp| {
            std::fs::write(tmp, "attacker-owned").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            tmp_tx.send(tmp.to_path_buf()).unwrap();
        }));

        let result = write_token_state(&path, &HashMap::new());

        *TOKEN_STATE_BEFORE_TEMP_CREATE_HOOK
            .lock()
            .expect("token-state before-create hook poisoned") = None;
        let tmp = tmp_rx.recv().unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(tmp).unwrap(), "attacker-owned");
    }

    #[test]
    fn token_state_write_failure_removes_temp_file() {
        let _serial = TOKEN_STATE_TEST_HOOK_LOCK
            .lock()
            .expect("token-state test hook lock poisoned");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("samsung-ip-tokens.json");
        *TOKEN_STATE_BEFORE_WRITE_FAILURE_HOOK
            .lock()
            .expect("token-state write-failure hook poisoned") = Some(Arc::new(|_| {
            Err(std::io::Error::other("forced write failure"))
        }));

        assert!(write_token_state(&path, &HashMap::new()).is_err());

        *TOKEN_STATE_BEFORE_WRITE_FAILURE_HOOK
            .lock()
            .expect("token-state write-failure hook poisoned") = None;
        let temp_prefix = ".samsung-ip-tokens.json.tmp.";
        assert!(
            std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(temp_prefix)),
            "failed token write must not leave a credential-bearing temp file"
        );
    }

    #[test]
    fn token_state_rename_failure_removes_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("samsung-ip-tokens.json");
        std::fs::create_dir(&path).unwrap();

        assert!(write_token_state(&path, &HashMap::new()).is_err());

        let temp_prefix = ".samsung-ip-tokens.json.tmp.";
        assert!(
            std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(temp_prefix)),
            "failed token rename must not leave a credential-bearing temp file"
        );
    }

    /// Keeps the cache mutex across the entire state-file update. Releasing it
    /// before the rename lets two same-process writes preserve only the last
    /// stale map, even when their temporary paths differ.
    #[test]
    fn concurrent_token_updates_keep_both_hosts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        let _serial = TOKEN_STATE_TEST_HOOK_LOCK
            .lock()
            .expect("token-state test hook lock poisoned");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("samsung-ip-tokens.json");
        let transport = Arc::new(
            RealBacklightTransport::for_test_with_state_path_without_file_lock(
                String::new(),
                Duration::from_secs(1),
                path.clone(),
            ),
        );

        let (second_started_tx, second_started_rx) = mpsc::channel();
        let (allow_second_tx, allow_second_rx) = mpsc::channel();
        let allow_second_rx = Arc::new(StdMutex::new(allow_second_rx));
        let before_cache_calls = Arc::new(AtomicUsize::new(0));
        let before_cache_path = path.clone();
        *TOKEN_STATE_BEFORE_CACHE_HOOK
            .lock()
            .expect("token-state before-cache hook poisoned") = Some(Arc::new(move |hook_path| {
            if hook_path != before_cache_path {
                return;
            }
            if before_cache_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                second_started_tx.send(()).unwrap();
                allow_second_rx.lock().unwrap().recv().unwrap();
            }
        }));

        let (first_loaded_tx, first_loaded_rx) = mpsc::channel();
        let (second_loaded_tx, second_loaded_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let (release_second_tx, release_second_rx) = mpsc::channel();
        let release_first_rx = Arc::new(StdMutex::new(release_first_rx));
        let release_second_rx = Arc::new(StdMutex::new(release_second_rx));
        let after_load_calls = Arc::new(AtomicUsize::new(0));
        let after_load_path = path.clone();
        *TOKEN_STATE_AFTER_LOAD_HOOK
            .lock()
            .expect("token-state after-load hook poisoned") = Some(Arc::new(move |hook_path| {
            if hook_path != after_load_path {
                return;
            }
            match after_load_calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    first_loaded_tx.send(()).unwrap();
                    release_first_rx.lock().unwrap().recv().unwrap();
                }
                1 => {
                    second_loaded_tx.send(()).unwrap();
                    release_second_rx.lock().unwrap().recv().unwrap();
                }
                n => panic!("unexpected token-state load hook call {n}"),
            }
        }));

        let first = Arc::clone(&transport);
        let first_writer = std::thread::spawn(move || first.persist_token("192.0.2.11", "token-a"));
        first_loaded_rx.recv().unwrap();

        let second = Arc::clone(&transport);
        let second_writer =
            std::thread::spawn(move || second.persist_token("192.0.2.12", "token-b"));
        second_started_rx.recv().unwrap();
        allow_second_tx.send(()).unwrap();

        // The timeout only converts a deadlock into a useful failure: the
        // channel gates force the order without a scheduling sleep.
        let second_reached_load_early = second_loaded_rx.recv_timeout(Duration::from_secs(1));
        release_first_tx.send(()).unwrap();
        assert!(first_writer.join().unwrap().is_ok());

        if second_reached_load_early.is_err() {
            second_loaded_rx.recv().unwrap();
        }
        release_second_tx.send(()).unwrap();
        assert!(second_writer.join().unwrap().is_ok());

        *TOKEN_STATE_BEFORE_CACHE_HOOK
            .lock()
            .expect("token-state before-cache hook poisoned") = None;
        *TOKEN_STATE_AFTER_LOAD_HOOK
            .lock()
            .expect("token-state after-load hook poisoned") = None;

        assert!(
            second_reached_load_early.is_err(),
            "the second writer reached the stale read before the first persisted"
        );
        let state = load_token_state(&path).unwrap();
        assert_eq!(state.get("192.0.2.11"), Some(&"token-a".to_string()));
        assert_eq!(state.get("192.0.2.12"), Some(&"token-b".to_string()));
    }

    /// Separate temporary files are required even with serialized state-map
    /// updates: sharing a temp path makes one writer rename the other's bytes.
    #[test]
    fn concurrent_token_state_writes_use_distinct_temp_files() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        let _serial = TOKEN_STATE_TEST_HOOK_LOCK
            .lock()
            .expect("token-state test hook lock poisoned");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("samsung-ip-tokens.json");
        let (first_ready_tx, first_ready_rx) = mpsc::channel();
        let (second_ready_tx, second_ready_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let (release_second_tx, release_second_rx) = mpsc::channel();
        let release_first_rx = Arc::new(StdMutex::new(release_first_rx));
        let release_second_rx = Arc::new(StdMutex::new(release_second_rx));
        let before_rename_calls = Arc::new(AtomicUsize::new(0));
        let before_rename_path = path.clone();
        *TOKEN_STATE_BEFORE_RENAME_HOOK
            .lock()
            .expect("token-state before-rename hook poisoned") = Some(Arc::new(move |hook_path| {
            if hook_path != before_rename_path {
                return;
            }
            match before_rename_calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    first_ready_tx.send(()).unwrap();
                    release_first_rx.lock().unwrap().recv().unwrap();
                }
                1 => {
                    second_ready_tx.send(()).unwrap();
                    release_second_rx.lock().unwrap().recv().unwrap();
                }
                n => panic!("unexpected token-state rename hook call {n}"),
            }
        }));

        let first_path = path.clone();
        let first_writer = std::thread::spawn(move || {
            let mut state = HashMap::new();
            state.insert("192.0.2.21".to_string(), "token-a".to_string());
            write_token_state(&first_path, &state)
        });
        first_ready_rx.recv().unwrap();

        let second_path = path.clone();
        let second_writer = std::thread::spawn(move || {
            let mut state = HashMap::new();
            state.insert("192.0.2.22".to_string(), "token-b".to_string());
            write_token_state(&second_path, &state)
        });
        second_ready_rx.recv().unwrap();

        release_first_tx.send(()).unwrap();
        assert!(first_writer.join().unwrap().is_ok());
        release_second_tx.send(()).unwrap();
        assert!(second_writer.join().unwrap().is_ok());

        *TOKEN_STATE_BEFORE_RENAME_HOOK
            .lock()
            .expect("token-state before-rename hook poisoned") = None;
    }

    /// `load_token_state` returns an empty map for a missing file (the
    /// common case on first daemon run) and a parse error for a
    /// malformed file (so the operator notices corruption rather than
    /// silently losing auth).
    #[test]
    fn load_token_state_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let map = load_token_state(&path).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn load_token_state_empty_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "").unwrap();
        let map = load_token_state(&path).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn load_token_state_malformed_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(load_token_state(&path).is_err());
    }

    #[tokio::test]
    async fn fake_acquire_token_records_host_and_returns_programmed() {
        let fake = FakeBacklightTransport::new();
        fake.acquire_results
            .lock()
            .unwrap()
            .push(Ok("tok-1".into()));
        fake.acquire_results
            .lock()
            .unwrap()
            .push(Err("nope".into()));

        assert_eq!(fake.acquire_token("192.0.2.7").await.unwrap(), "tok-1");
        assert!(fake.acquire_token("192.0.2.7").await.is_err());
        assert_eq!(
            *fake.acquire_hosts.lock().unwrap(),
            vec!["192.0.2.7", "192.0.2.7"]
        );
    }

    #[tokio::test]
    async fn fake_default_acquire_returns_fake_token() {
        let fake = FakeBacklightTransport::new();
        assert_eq!(fake.acquire_token("h").await.unwrap(), "fake-token");
    }

    #[tokio::test]
    async fn fake_get_backlight_default_returns_40() {
        let fake = FakeBacklightTransport::new();
        assert_eq!(fake.get_backlight("h", "t").await.unwrap(), 40);
    }

    #[tokio::test]
    async fn fake_set_backlight_records_call_and_default_ok() {
        let fake = FakeBacklightTransport::new();
        fake.set_backlight("h", "t", 0).await.unwrap();
        fake.set_backlight("h", "t", 12).await.unwrap();
        assert_eq!(
            *fake.set_calls.lock().unwrap(),
            vec![("h".to_string(), 0), ("h".to_string(), 12)]
        );
    }

    #[tokio::test]
    async fn fake_input_source_records_host_and_token_and_consumes_script() {
        let fake = FakeBacklightTransport::new();
        fake.input_source_results
            .lock()
            .unwrap()
            .push(Ok("HDMI1".to_string()));
        fake.input_source_results
            .lock()
            .unwrap()
            .push(Ok("HDMI4".to_string()));

        assert_eq!(
            fake.input_source("192.0.2.7", "tok-1").await.unwrap(),
            "HDMI1"
        );
        assert_eq!(
            fake.input_source("192.0.2.8", "tok-2").await.unwrap(),
            "HDMI4"
        );
        assert_eq!(
            *fake.input_source_calls.lock().unwrap(),
            vec![
                ("192.0.2.7".to_string(), "tok-1".to_string()),
                ("192.0.2.8".to_string(), "tok-2".to_string()),
            ]
        );
    }

    #[test]
    fn map_transport_error_unauthorized_includes_code_and_e_display_io() {
        let fake = FakeBacklightTransport::new();
        let err = map_transport_error("samsung-tizen", &fake, "192.0.2.7", "-32010 token bad");
        assert_eq!(err.controller, "samsung-tizen");
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains(E_JSONRPC_UNAUTHORIZED));
    }

    #[test]
    fn map_transport_error_other_codes_include_classified_anchor() {
        let fake = FakeBacklightTransport::new();
        let err = map_transport_error("samsung-tizen", &fake, "h", "-32002 locked");
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains(E_JSONRPC_FAILED_OR_LOCKED));
    }

    /// JSON-RPC error response body → classified anchor in the surfaced
    /// `CmdFailure`. Guards against a regression where the raw `HTTP request
    /// failed: <err>` opaque string leaked past the JSON-RPC parser.
    #[tokio::test]
    async fn real_transport_surfaces_jsonrpc_error_with_classified_anchor() {
        // reqwest::Client::danger_accept_invalid_certs + a self-signed cert
        // is not exercisable here (no self-signed cert in this test), so
        // verify the helper path directly via the same error-parsing the
        // transport performs. The full reqwest round-trip against a
        // wiremock self-signed server is exercised by the
        // `backlight_http_request_shape_and_error_mapping` test below.
        let err = map_transport_error(
            "samsung-tizen",
            &FakeBacklightTransport::new(),
            "192.0.2.7",
            "-32601 method not found",
        );
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains(E_JSONRPC_METHOD_NOT_FOUND));
    }

    /// Drive `RealBacklightTransport` end-to-end through a wiremock server.
    /// Proves (a) the wire shape (JSON-RPC POST with method + params),
    /// (b) the token cache — the second call to `acquire_token` does NOT
    /// re-hit the server, (c) error mapping for a `-32601` JSON-RPC
    /// response, (d) `-32010` triggers `invalidate_token` so the next
    /// `acquire_token` re-acquires.
    #[tokio::test]
    async fn real_transport_full_round_trip_via_wiremock() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let base_url = mock.uri();

        // createAccessToken: returns a token, expected ONCE.
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "AccessToken": "tok-1" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // backlightControl: returns the echoed value.
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "method": "backlightControl",
                "params": { "backlight": 25 }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "backlight": 25 }
            })))
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(base_url, Duration::from_secs(5));

        // First acquire: hits the mock, returns tok-1, caches it.
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(tok, "tok-1");

        // Second acquire: cache hit, NO additional HTTP request to the mock.
        let tok_again = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(tok_again, "tok-1");

        // set_backlight uses the cached token; mock echoes back 25.
        transport
            .set_backlight("192.0.2.7", &tok, 25)
            .await
            .unwrap();
    }

    /// JSON-RPC error response body is parsed and the literal code is
    /// preserved in the surfaced `String` — proves the error-mapping
    /// pipeline that `map_transport_error` later classifies.
    #[tokio::test]
    async fn real_transport_jsonrpc_error_response_surfaces_raw_code() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": { "code": -32601, "message": "method not found" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));

        let err = transport
            .acquire_token("192.0.2.7")
            .await
            .expect_err("expected JSON-RPC error");
        // The code is preserved as the leading digit run.
        assert!(
            err.starts_with("-32601"),
            "raw error should preserve the JSON-RPC code: {err}"
        );
        assert!(err.contains("method not found"));
    }

    /// `-32010` triggers `invalidate_token`, so the next `acquire_token`
    /// re-acquires from the server (cache miss after invalidation).
    #[tokio::test]
    async fn real_transport_unauthorized_invalidates_cache_and_reacquires() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // First createAccessToken: returns tok-A. Retired after the first hit so
        // the second createAccessToken falls through to the tok-B mock.
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "AccessToken": "tok-A" }
            })))
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // First backlightControl: returns -32010 (unauthorized).
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "method": "backlightControl"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": { "code": -32010, "message": "token rejected" }
            })))
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // Second createAccessToken: returns tok-B (re-acquire after
        // invalidation).
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "AccessToken": "tok-B" }
            })))
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));

        // Prime the cache.
        let tok_a = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(tok_a, "tok-A");

        // set_backlight with tok-A fails (-32010) — controller would call
        // map_transport_error which calls invalidate_token; here we
        // simulate that step directly.
        let err = transport
            .set_backlight("192.0.2.7", &tok_a, 0)
            .await
            .expect_err("expected -32010");
        assert!(err.contains("-32010"));
        transport.invalidate_token_for_test("192.0.2.7");

        // Next acquire: cache is empty (invalidated), so a second HTTP
        // request to the mock returns tok-B.
        let tok_b = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(tok_b, "tok-B");
    }

    /// Backlight reads go through `backlightControl` with the token and
    /// NO `backlight` field — the proven wire shape. Parses the
    /// `result.backlight` field as a `u8`. Out-of-range or missing values
    /// produce a typed error.
    #[tokio::test]
    async fn real_transport_get_backlight_parses_result_backlight() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "AccessToken": "tok" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "method": "backlightControl",
                "params": { "AccessToken": "tok" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "backlight": 37 }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        let value = transport.get_backlight("192.0.2.7", &tok).await.unwrap();
        assert_eq!(value, 37);
    }

    #[tokio::test]
    async fn real_transport_input_source_uses_control_method_and_parses_string() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("accept", "application/json"))
            .and(body_partial_json(json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "AccessToken": "tok" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("accept", "application/json"))
            .and(body_partial_json(json!({
                "method": "inputSourceControl",
                "params": { "AccessToken": "tok" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "inputSource": "HDMI4" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));
        let token = transport.acquire_token("192.0.2.7").await.unwrap();

        assert_eq!(
            transport.input_source("192.0.2.7", &token).await.unwrap(),
            "HDMI4"
        );
    }

    #[tokio::test]
    async fn real_transport_input_source_rejects_missing_or_non_string_result() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let cases = [
            ("missing result", json!({})),
            ("missing inputSource", json!({ "result": {} })),
            (
                "non-string inputSource",
                json!({ "result": { "inputSource": 4 } }),
            ),
            (
                "empty inputSource",
                json!({ "result": { "inputSource": "" } }),
            ),
        ];

        for (name, response) in cases {
            let mock = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/"))
                .and(header("accept", "application/json"))
                .and(body_partial_json(json!({
                    "method": "inputSourceControl",
                    "params": { "AccessToken": "tok" }
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .expect(1)
                .mount(&mock)
                .await;

            let transport =
                RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));
            let result = transport.input_source("192.0.2.7", "tok").await;

            assert!(result.is_err(), "{name}: expected malformed result to fail");
        }
    }

    /// Pin the `Accept: application/json` header on every port-1516 POST.
    ///
    /// The real Samsung TV returns HTTP 400 unless the request carries
    /// `Accept: application/json` — reqwest's default `Accept: */*` is
    /// rejected. The other round-trip tests use wiremock mocks that match
    /// any Accept, so they passed before this regression was caught and
    /// would silently pass again if the header were dropped.
    ///
    /// This test guards both mocks with
    /// `wiremock::matchers::header("accept", "application/json")` (wiremock
    /// matches header names case-insensitively, hence the lowercase) and
    /// mounts NO fallback. With the fix in place the mocks match and the
    /// round-trip returns 37; without it wiremock returns its default 404,
    /// `call()` surfaces `HTTP 404 Bad Request`, and `unwrap()` panics.
    #[tokio::test]
    async fn real_transport_pins_accept_application_json_header() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("accept", "application/json"))
            .and(body_partial_json(serde_json::json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "AccessToken": "tok" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("accept", "application/json"))
            .and(body_partial_json(serde_json::json!({
                "method": "backlightControl",
                "params": { "AccessToken": "tok" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "backlight": 37 }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        let value = transport.get_backlight("192.0.2.7", &tok).await.unwrap();
        assert_eq!(value, 37);
    }

    // ── PROVEN-WIRE-SHAPE TESTS — pin the request/response bodies the real
    //    Samsung TV accepts on port 1516. The previous tests mocked the
    //    wire shape we GUESSED (params:{} for createAccessToken,
    //    getVideoStates for reads) — and passed — while the real TV rejected
    //    those requests with HTTP 400. These tests mock the PROVEN shape and
    //    would fail against the old code.

    /// `createAccessToken` request body MUST NOT include a `params` key —
    /// the real TV returns HTTP 400 when `params: {}` is present. The fix
    /// is to omit the key entirely. Uses `body_json` (exact match) so any
    /// stray `params` key, including an empty object, fails the matcher.
    #[tokio::test]
    async fn real_transport_create_access_token_request_omits_params_key() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_json(json!({
                "jsonrpc": "2.0",
                "method": "createAccessToken",
                "id": 1
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "AccessToken": "tok-no-params" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(tok, "tok-no-params");
    }

    /// `get_backlight` MUST call `backlightControl` with the token and NO
    /// `backlight` field — NOT `getVideoStates` (which has no backlight
    /// field on the real TV and so always failed). Pins both the method
    /// and the absence of a `backlight` key in the params.
    #[tokio::test]
    async fn real_transport_get_backlight_calls_backlight_control_with_token_only() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "AccessToken": "tok-1" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // The read MUST be backlightControl with token and NO backlight
        // field. We assert the partial structure (method + params shape).
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({
                "method": "backlightControl",
                "params": { "AccessToken": "tok-1" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "backlight": 42 }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        let value = transport.get_backlight("192.0.2.7", &tok).await.unwrap();
        assert_eq!(value, 42);
    }

    /// `set_backlight` MUST call `backlightControl` with the token AND a
    /// `backlight` field — the proven wire shape.
    #[tokio::test]
    async fn real_transport_set_backlight_calls_backlight_control_with_token_and_value() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({
                "method": "createAccessToken"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "AccessToken": "tok-1" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({
                "method": "backlightControl",
                "params": { "AccessToken": "tok-1", "backlight": 17 }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "backlight": 17 }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let transport =
            RealBacklightTransport::for_test_with_base_url(mock.uri(), Duration::from_secs(5));
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        transport
            .set_backlight("192.0.2.7", &tok, 17)
            .await
            .unwrap();
    }

    // ── TOKEN-PERSISTENCE TESTS — re-acquiring the token on every daemon
    //    restart is unreliable on the real TV (intermittent on-screen
    //    allow). Persist the token to a 0600 state file so a known-good
    //    token survives restarts.

    /// A transport constructed against a state file pre-populated with a
    /// token MUST reuse that token WITHOUT hitting `createAccessToken` on
    /// the server. (If `createAccessToken` were called, the mock would
    /// return a different token; we assert it's the persisted one.)
    #[tokio::test]
    async fn real_transport_reuses_persisted_token_without_create_access_token() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // createAccessToken is mounted but with expect(0): a hit would be
        // a test failure. If the production code re-acquires on startup,
        // wiremock sees a request and fails the test.
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({ "method": "createAccessToken" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "AccessToken": "should-not-be-used" }
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("samsung-ip-tokens.json");
        std::fs::write(&state_path, r#"{"192.0.2.7":"persisted-tok-xyz"}"#).unwrap();

        let transport = RealBacklightTransport::for_test_with_state_path(
            mock.uri(),
            Duration::from_secs(5),
            state_path,
        );
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(
            tok, "persisted-tok-xyz",
            "transport must reuse the persisted token instead of calling createAccessToken"
        );
    }

    /// A successful `acquire_token` MUST persist the token to the state
    /// file so it survives the next daemon restart.
    #[tokio::test]
    async fn real_transport_persists_acquired_token_to_state_file() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({ "method": "createAccessToken" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "AccessToken": "freshly-acquired-tok" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("samsung-ip-tokens.json");
        assert!(!state_path.exists(), "state file should not exist yet");

        let transport = RealBacklightTransport::for_test_with_state_path(
            mock.uri(),
            Duration::from_secs(5),
            state_path.clone(),
        );
        let tok = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(tok, "freshly-acquired-tok");

        let raw = std::fs::read_to_string(&state_path).unwrap();
        assert!(
            raw.contains("freshly-acquired-tok"),
            "state file must persist the acquired token: {raw}"
        );
        assert!(
            raw.contains("192.0.2.7"),
            "state file must persist the host key: {raw}"
        );
    }

    /// A `-32010` unauthorized response invalidates BOTH the in-memory
    /// cache and the persisted entry — the next `acquire_token` calls
    /// `createAccessToken` and overwrites the persisted entry.
    #[tokio::test]
    async fn real_transport_unauthorized_invalidates_persisted_token() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // createAccessToken: returns fresh-tok (re-acquire after
        // invalidation of the persisted stale-tok).
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({ "method": "createAccessToken" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "AccessToken": "fresh-tok" }
            })))
            .mount(&mock)
            .await;

        // First backlightControl: -32010 (unauthorized).
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({ "method": "backlightControl" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": { "code": -32010, "message": "token rejected" }
            })))
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // Pre-populate the persisted token so the first acquire reuses it.
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("samsung-ip-tokens.json");
        std::fs::write(&state_path, r#"{"192.0.2.7":"stale-tok"}"#).unwrap();

        let transport = RealBacklightTransport::for_test_with_state_path(
            mock.uri(),
            Duration::from_secs(5),
            state_path.clone(),
        );

        // acquire_token reuses the persisted stale-tok (no HTTP hit yet).
        let stale = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(stale, "stale-tok");

        // set_backlight with the stale tok hits -32010. The controller
        // would call map_transport_error → invalidate_token; we simulate
        // it directly.
        let err = transport
            .set_backlight("192.0.2.7", &stale, 0)
            .await
            .expect_err("expected -32010");
        assert!(err.contains("-32010"));
        transport.invalidate_token_for_test("192.0.2.7");

        // Persisted entry for 192.0.2.7 must have been dropped by
        // invalidate_token (the spec asks invalidate to also drop the
        // persisted entry so a re-acquire writes a fresh token).
        let raw = std::fs::read_to_string(&state_path).unwrap();
        assert!(
            !raw.contains("stale-tok"),
            "stale token must be removed from the state file on -32010: {raw}"
        );

        // Next acquire re-acquires (hits createAccessToken) and the
        // freshly-returned token is persisted.
        let fresh = transport.acquire_token("192.0.2.7").await.unwrap();
        assert_eq!(fresh, "fresh-tok");
        let raw = std::fs::read_to_string(&state_path).unwrap();
        assert!(
            raw.contains("fresh-tok"),
            "fresh token must be persisted: {raw}"
        );
    }

    // ── AppVisibilityProbe fake tests ────────────────────────────────────────────

    /// `FakeAppVisibilityProbe` returns the programmed outcome and
    /// records host + app id on each probe.
    #[tokio::test]
    async fn fake_app_probe_set_returns_value_and_records_call() {
        let fake = FakeAppVisibilityProbe::new();
        fake.set("3201512006963", AppVisibility::Visible);
        assert_eq!(
            fake.probe("tv.local", "3201512006963").await,
            AppVisibility::Visible
        );
        assert_eq!(
            &*fake.calls.lock().unwrap(),
            &[("tv.local".to_owned(), "3201512006963".to_owned())]
        );
    }

    /// An unconfigured app id is treated as `NotVisible` (the real
    /// probe's 404 path) so tests can spot-configure a single app
    /// without scripting every catalog entry.
    #[tokio::test]
    async fn fake_app_probe_unset_id_defaults_to_not_visible() {
        let fake = FakeAppVisibilityProbe::new();
        assert_eq!(
            fake.probe("tv.local", "111299001912").await,
            AppVisibility::NotVisible
        );
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }

    /// `force_unknown` overrides every outcome to `Unknown`, simulating
    /// a fully-unreachable 8001 endpoint. Used by
    /// `source_gate` tests to verify the fail-safe direction.
    #[tokio::test]
    async fn fake_app_probe_force_unknown_overrides_outcomes() {
        let fake = FakeAppVisibilityProbe::new();
        fake.set("visible-app", AppVisibility::Visible);
        fake.set("not-visible-app", AppVisibility::NotVisible);
        *fake.force_unknown.lock().unwrap() = true;
        assert_eq!(
            fake.probe("tv.local", "visible-app").await,
            AppVisibility::Unknown,
            "force_unknown must override even a programmed Visible outcome"
        );
        assert_eq!(
            fake.probe("tv.local", "missing-app").await,
            AppVisibility::Unknown,
            "force_unknown applies to unconfigured ids too"
        );
        assert_eq!(
            fake.probe("tv.local", "not-visible-app").await,
            AppVisibility::Unknown
        );
    }

    /// `RealAppVisibilityProbe` is exercised end-to-end through
    /// `wiremock` to confirm the wire shape: `GET /api/v2/applications/<id>`,
    /// no auth header, plain HTTP. The 200/404/500 outcomes exercise
    /// the three wire paths (`Visible`, `NotVisible` via 404, `Unknown` via
    /// 5xx).
    #[tokio::test]
    async fn real_app_probe_parses_visible_true_and_404_returns_not_visible() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Happy path: app installed and on screen.
        Mock::given(method("GET"))
            .and(path("/api/v2/applications/3201512006963"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "YouTube",
                "running": true,
                "visible": true
            })))
            .mount(&server)
            .await;
        // 404 for an app id the TV doesn't recognize — must surface as
        // NotVisible, NOT Unknown. A 404 is a definitive answer.
        Mock::given(method("GET"))
            .and(path("/api/v2/applications/uninstalled"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // 500 surfaces as Unknown (server-side problem, retry next cycle).
        Mock::given(method("GET"))
            .and(path("/api/v2/applications/flaky"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let probe = RealAppVisibilityProbe::for_test_with_base_url(server.uri());
        assert_eq!(
            probe.probe("ignored-in-test", "3201512006963").await,
            AppVisibility::Visible,
            "200 with visible=true must yield Visible"
        );
        assert_eq!(
            probe.probe("ignored-in-test", "uninstalled").await,
            AppVisibility::NotVisible,
            "404 must surface as NotVisible (definitive non-presence)"
        );
        assert_eq!(
            probe.probe("ignored-in-test", "flaky").await,
            AppVisibility::Unknown,
            "5xx must surface as Unknown (transient — retry next cycle)"
        );
    }

    /// Real probe treats a JSON body without `visible` as `Unknown`
    /// rather than inferring visibility from `running`. Older Tizen
    /// firmware may omit the field — the fail-safe direction is to
    /// not assume `NotVisible` for ambiguous shapes.
    #[tokio::test]
    async fn real_app_probe_treats_missing_visible_field_as_unknown() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/applications/old-firmware"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "MysteryApp",
                "running": true
            })))
            .mount(&server)
            .await;
        let probe = RealAppVisibilityProbe::for_test_with_base_url(server.uri());
        assert_eq!(
            probe.probe("ignored-in-test", "old-firmware").await,
            AppVisibility::Unknown,
            "missing visible field must surface as Unknown, not NotVisible"
        );
    }
}
