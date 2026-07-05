//! Abstract DDC/CI operations (`VcpOps` trait), a real implementation backed by
//! ddc-hi (wrapped in `spawn_blocking`), and a scripted fake for unit tests.
//!
//! ## Design
//!
//! Every hardware touch is wrapped in [`tokio::task::spawn_blocking`] so the
//! async executor is never blocked. No ddc-hi `Display` handle is cached across
//! operations — each call re-enumerates, finds the matching display, performs
//! the op, and drops the handle. Enumeration is ~100 ms, which is acceptable
//! at blank/wake frequency.
//!
//! The trait is `#[async_trait]` so that the real implementation can `await`
//! the `spawn_blocking` join handle directly, avoiding the
//! `block_in_place`/`block_on` triple-wrap that panics on current-thread
//! runtimes.

use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
#[cfg(target_os = "linux")]
use ddc_hi::Ddc;

/// Information about a detected display returned by [`VcpOps::list_displays`].
#[derive(Debug, Clone)]
pub struct VcpDisplayInfo {
    /// Human-readable identifier string (backend:id manufacturer `model_name`).
    pub ident_string: String,
}

/// Abstract DDC/CI operations — real or fake.
///
/// Every method is `Send + Sync` so the trait object can be shared across
/// async tasks. The real implementation wraps blocking ddc-hi calls in
/// [`tokio::task::spawn_blocking`].
///
/// Methods are async so the real implementation can `await` the blocking
/// task directly without `block_in_place`/`block_on` gymnastics.
#[async_trait]
pub trait VcpOps: Send + Sync {
    /// Enumerate all DDC/CI-capable displays.
    async fn list_displays(&self) -> Vec<VcpDisplayInfo>;

    /// Get the current value of a VCP feature code.
    ///
    /// # Errors
    ///
    /// Returns an error string if the VCP read fails (I/O error, display
    /// disconnected, or unsupported feature code).
    async fn get_vcp(&self, ident: &str, code: u8) -> Result<u16, String>;

    /// Set a VCP feature code to a value.
    ///
    /// # Errors
    ///
    /// Returns an error string if the VCP write fails (I/O error, display
    /// disconnected, or unsupported feature code).
    async fn set_vcp(&self, ident: &str, code: u8, value: u16) -> Result<(), String>;
}

// ── RealVcp — wraps ddc-hi in spawn_blocking ───────────────────────────────────

/// Real DDC/CI operations backed by ddc-hi, with every call wrapped in
/// [`tokio::task::spawn_blocking`].
///
/// Only available on Linux — DDC/CI I²C access requires platform support.
#[cfg(target_os = "linux")]
pub struct RealVcp;

#[cfg(target_os = "linux")]
impl RealVcp {
    /// Enumerate synchronously (called inside `spawn_blocking`).
    fn enumerate_displays() -> Vec<(String, ddc_hi::Display)> {
        ddc_hi::Display::enumerate()
            .into_iter()
            .map(|d| (d.info.to_string(), d))
            .collect()
    }

    /// Find a display by ident string from an enumerated list.
    fn find_display<'a>(
        ident: &str,
        displays: &'a mut [(String, ddc_hi::Display)],
    ) -> Result<&'a mut ddc_hi::Display, String> {
        displays
            .iter_mut()
            .find(|(id, _)| id == ident)
            .map(|(_, d)| d)
            .ok_or_else(|| format!("display '{ident}' not found during re-enumeration"))
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl VcpOps for RealVcp {
    async fn list_displays(&self) -> Vec<VcpDisplayInfo> {
        let displays = tokio::task::spawn_blocking(ddc_hi::Display::enumerate)
            .await
            .unwrap_or_default();
        displays
            .into_iter()
            .map(|d| VcpDisplayInfo {
                ident_string: d.info.to_string(),
            })
            .collect()
    }

    async fn get_vcp(&self, ident: &str, code: u8) -> Result<u16, String> {
        let ident = ident.to_string();
        tokio::task::spawn_blocking(move || {
            let mut displays = Self::enumerate_displays();
            let display = Self::find_display(&ident, &mut displays)?;
            let vcp = display
                .handle
                .get_vcp_feature(code)
                .map_err(|e| format!("get_vcp(0x{code:02X}) failed: {e}"))?;
            Ok::<u16, String>(vcp.value())
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }

    async fn set_vcp(&self, ident: &str, code: u8, value: u16) -> Result<(), String> {
        let ident = ident.to_string();
        tokio::task::spawn_blocking(move || {
            let mut displays = Self::enumerate_displays();
            let display = Self::find_display(&ident, &mut displays)?;
            display
                .handle
                .set_vcp_feature(code, value)
                .map_err(|e| format!("set_vcp(0x{code:02X}, {value}) failed: {e}"))
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }
}

// ── FakeVcp — scripted operations for tests ────────────────────────────────────

/// A scripted [`VcpOps`] implementation for unit tests.
///
/// Each call records its arguments in a call log (accessible via
/// `take_call_log`) and returns values from a pre-configured script.
/// All mutable state is behind [`StdMutex`] so the trait's `&self` methods
/// can mutate script state and the call log.
///
/// This type is `pub(crate)` and only used by the `ddcci` test module, so
/// it is dead code in non-test builds.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct FakeVcp {
    displays: Vec<VcpDisplayInfo>,
    /// (ident, code) → Result<value, err>
    get_script: StdMutex<Vec<ScriptEntry>>,
    /// (ident, code, value) → Result<(), err>
    set_script: StdMutex<Vec<SetScriptEntry>>,
    call_log: StdMutex<Vec<String>>,
}

/// A single scripted `get_vcp` response.
#[cfg_attr(not(test), allow(dead_code))]
type ScriptEntry = ((String, u8), Result<u16, String>);

/// A single scripted `set_vcp` response.
#[cfg_attr(not(test), allow(dead_code))]
type SetScriptEntry = ((String, u8, u16), Result<(), String>);

#[cfg_attr(not(test), allow(dead_code))]
impl FakeVcp {
    /// Create a new `FakeVcp` with the given displays.
    #[must_use]
    pub fn new(displays: Vec<VcpDisplayInfo>) -> Self {
        Self {
            displays,
            get_script: StdMutex::new(Vec::new()),
            set_script: StdMutex::new(Vec::new()),
            call_log: StdMutex::new(Vec::new()),
        }
    }

    /// Add a scripted `get_vcp` response.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_get(&self, ident: &str, code: u8, result: Result<u16, String>) {
        self.get_script
            .lock()
            .unwrap()
            .push(((ident.to_string(), code), result));
    }

    /// Add a scripted `set_vcp` response.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn expect_set(&self, ident: &str, code: u8, value: u16, result: Result<(), String>) {
        self.set_script
            .lock()
            .unwrap()
            .push(((ident.to_string(), code, value), result));
    }

    /// Drain the call log (FIFO).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn take_call_log(&self) -> Vec<String> {
        let mut log = self.call_log.lock().unwrap();
        std::mem::take(&mut *log)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[async_trait]
impl VcpOps for FakeVcp {
    async fn list_displays(&self) -> Vec<VcpDisplayInfo> {
        self.displays.clone()
    }

    async fn get_vcp(&self, ident: &str, code: u8) -> Result<u16, String> {
        self.call_log
            .lock()
            .unwrap()
            .push(format!("get_vcp({ident}, 0x{code:02X})"));
        let mut script = self.get_script.lock().unwrap();
        let idx = script
            .iter()
            .position(|((id, c), _)| id == ident && *c == code);
        match idx {
            Some(i) => {
                let ((_, _), result) = script.remove(i);
                result
            }
            None => Err(format!(
                "FakeVcp: no scripted response for get_vcp({ident}, 0x{code:02X})"
            )),
        }
    }

    async fn set_vcp(&self, ident: &str, code: u8, value: u16) -> Result<(), String> {
        self.call_log
            .lock()
            .unwrap()
            .push(format!("set_vcp({ident}, 0x{code:02X}, {value})"));
        let mut script = self.set_script.lock().unwrap();
        let idx = script
            .iter()
            .position(|((id, c, v), _)| id == ident && *c == code && *v == value);
        match idx {
            Some(i) => {
                let ((_, _, _), result) = script.remove(i);
                result
            }
            None => Err(format!(
                "FakeVcp: no scripted response for set_vcp({ident}, 0x{code:02X}, {value})"
            )),
        }
    }
}
