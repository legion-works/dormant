//! Best-effort `sd_notify(3)` over `NOTIFY_SOCKET` (spec §6.2).
//!
//! Zero deps: talks to systemd's local `SOCK_DGRAM` notification socket
//! directly. Absent `NOTIFY_SOCKET` ⇒ permanent no-op (no fd ever opened).
//! A send failure is debug-logged exactly once (`sd_notify_unavailable`)
//! and permanently disables further attempts — reusing the SAME no-op path
//! already used for "absent", so no separate disabled-flag state is
//! needed: once a send fails we simply drop the socket/target back to
//! `None`.
//!
//! Non-Linux targets get a fully inert stub (same public surface, every
//! method a no-op) mirroring the `idle_source.rs:289-298` per-function cfg
//! shape — the abstract-namespace socket support this module needs
//! (`std::os::linux::net::SocketAddrExt`, `UnixDatagram::{bind_addr,
//! send_to_addr}`) is Linux-only.
//!
//! `SdNotify` is deliberately NOT `Clone`/`Arc`-shared: it has exactly one
//! owner at a time along the boot chain (`BootInputs::sd_notify` →
//! `App::with_sd_notify` → `Runner`, spec §6.2/§10) and is moved end to
//! end, never fanned out to concurrent tasks. Wrapping it in `Arc` (and the
//! interior-mutable disable flag that would then require) would add
//! complexity with no consumer — see the report for the full justification.

use std::time::Duration;

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::ffi::OsStr;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    use std::path::PathBuf;

    /// Where a notification datagram goes: a plain filesystem path (sent via
    /// the ordinary cross-platform [`UnixDatagram::send_to`]) or a Linux
    /// abstract-namespace name (spec F19 — sent via the Linux-only
    /// [`UnixDatagram::send_to_addr`]; a naive `send_to("@name")` would
    /// silently try to open a *file* literally named `@name` instead).
    #[derive(Debug)]
    enum Target {
        Path(PathBuf),
        Abstract(SocketAddr),
    }

    /// Best-effort `sd_notify` sender. `sock`/`target` are `None` together —
    /// both at construction (absent/unusable `NOTIFY_SOCKET`) and forever
    /// after the first failed send (spec: debug-log once, then disable).
    pub struct SdNotify {
        sock: Option<UnixDatagram>,
        target: Option<Target>,
    }

    impl SdNotify {
        /// Reads `NOTIFY_SOCKET` now. Absent/empty/unusable ⇒ permanent
        /// no-op.
        #[must_use]
        pub fn from_env() -> Self {
            match std::env::var_os("NOTIFY_SOCKET") {
                Some(v) if !v.is_empty() => Self::from_bytes(v.as_bytes()),
                _ => Self::disabled(),
            }
        }

        /// Test seam (R2-M8): `NOTIFY_SOCKET` is process-global and races
        /// under concurrent test execution, so tests inject a target
        /// address directly instead of setting the env var.
        ///
        /// Gated on `any(test, feature = "test-util")` rather than bare
        /// `cfg(test)` (T4 fix): an external integration test binary
        /// (`tests/daemon_smoke.rs`) links the library WITHOUT `--cfg
        /// test` — only the crate's OWN `cargo test` lib-unittest build
        /// gets that cfg — so `cfg(test)` alone left this seam invisible
        /// to `daemon_smoke`'s watchdog-ping-cadence tests. The `test-util`
        /// feature is enabled only via `dormantd`'s own
        /// `[dev-dependencies]` self-reference (Cargo.toml), never by a
        /// production build.
        #[cfg(any(test, feature = "test-util"))]
        #[must_use]
        pub fn from_socket_for_test(addr: &SocketAddr) -> Self {
            let Ok(sock) = UnixDatagram::unbound() else {
                return Self::disabled();
            };
            if let Some(path) = addr.as_pathname() {
                Self {
                    sock: Some(sock),
                    target: Some(Target::Path(path.to_path_buf())),
                }
            } else if let Some(name) = addr.as_abstract_name() {
                match SocketAddr::from_abstract_name(name) {
                    Ok(a) => Self {
                        sock: Some(sock),
                        target: Some(Target::Abstract(a)),
                    },
                    Err(_) => Self::disabled(),
                }
            } else {
                Self::disabled()
            }
        }

        // `pub(super)`: reachable from the co-located `tests` module (a
        // sibling of `linux_impl` under `sd_notify`), not part of the
        // crate-external public surface.
        pub(super) fn disabled() -> Self {
            Self {
                sock: None,
                target: None,
            }
        }

        /// Shared parsing core for `from_env`'s raw `NOTIFY_SOCKET` bytes —
        /// exercised indirectly by the `from_socket_for_test`-driven tests
        /// below (both paths build the same `Target` shapes), since real
        /// `NOTIFY_SOCKET` env manipulation is deliberately not
        /// unit-tested (R2-M8).
        fn from_bytes(bytes: &[u8]) -> Self {
            let Ok(sock) = UnixDatagram::unbound() else {
                return Self::disabled();
            };
            if let Some(name) = bytes.strip_prefix(b"@") {
                match SocketAddr::from_abstract_name(name) {
                    Ok(addr) => Self {
                        sock: Some(sock),
                        target: Some(Target::Abstract(addr)),
                    },
                    Err(_) => Self::disabled(),
                }
            } else {
                Self {
                    sock: Some(sock),
                    target: Some(Target::Path(PathBuf::from(OsStr::from_bytes(bytes)))),
                }
            }
        }

        fn send(&mut self, msg: &[u8]) {
            let (Some(sock), Some(target)) = (self.sock.as_ref(), self.target.as_ref()) else {
                return;
            };
            let result = match target {
                Target::Path(p) => sock.send_to(msg, p),
                Target::Abstract(a) => sock.send_to_addr(msg, a),
            };
            if result.is_err() {
                tracing::debug!(
                    event = "sd_notify_unavailable",
                    "sd_notify send failed; disabling further attempts"
                );
                self.sock = None;
                self.target = None;
            }
        }

        /// Sends `READY=1`.
        pub fn ready(&mut self) {
            self.send(b"READY=1");
        }

        /// Sends `WATCHDOG=1`.
        pub fn watchdog(&mut self) {
            self.send(b"WATCHDOG=1");
        }

        #[cfg(test)]
        pub(super) fn is_disabled(&self) -> bool {
            self.sock.is_none() && self.target.is_none()
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux_impl::SdNotify;

/// Non-Linux stub — no abstract-namespace socket support in std; the whole
/// surface is a permanent no-op (`idle_source.rs:289-298` cfg shape).
#[cfg(not(target_os = "linux"))]
pub struct SdNotify;

#[cfg(not(target_os = "linux"))]
impl SdNotify {
    #[must_use]
    pub fn from_env() -> Self {
        Self
    }

    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn from_socket_for_test(_addr: &std::os::unix::net::SocketAddr) -> Self {
        Self
    }

    pub fn ready(&mut self) {}

    pub fn watchdog(&mut self) {}
}

/// Parses `WATCHDOG_USEC` and returns HALF the interval (systemd
/// convention: ping at least twice per `WatchdogSec`). Absent or
/// unparsable (garbage, zero) ⇒ `None`.
#[must_use]
pub fn watchdog_interval_from_env() -> Option<Duration> {
    let raw = std::env::var("WATCHDOG_USEC").ok()?;
    let usec: u64 = raw.trim().parse().ok()?;
    if usec == 0 {
        return None;
    }
    Some(Duration::from_micros(usec) / 2)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux_impl::SdNotify;
    use super::watchdog_interval_from_env;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    use std::sync::Mutex;
    use std::time::Duration;

    // Only `watchdog_interval_from_env` touches process-global env, and
    // only under this mutex (Global Constraints: NEVER `set_var` for
    // NOTIFY_SOCKET/WATCHDOG_USEC outside the two serialized parse cases).
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn env_absent_is_noop_construct() {
        // None-forcing test path — NOT via env (R2-M8): `disabled()` is
        // private but reachable from this co-located test module.
        let mut sd = SdNotify::disabled();
        assert!(sd.is_disabled());
        // Must be safe to call — no panic, no fd, no-op.
        sd.ready();
        sd.watchdog();
        assert!(sd.is_disabled());
    }

    #[test]
    fn temp_path_socket_receives_ready_then_watchdog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let addr = SocketAddr::from_pathname(&path).unwrap();
        let mut sd = SdNotify::from_socket_for_test(&addr);

        sd.ready();
        sd.watchdog();

        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }

    #[test]
    fn abstract_name_socket_receives_ready_then_watchdog() {
        // Unique-per-test-run abstract name so parallel test threads don't
        // collide on the same kernel-global abstract namespace.
        let name = format!(
            "dormantd-sd-notify-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let bind_addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixDatagram::bind_addr(&bind_addr).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let mut sd = SdNotify::from_socket_for_test(&bind_addr);

        sd.ready();
        sd.watchdog();

        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }

    #[test]
    fn send_failure_disables_further_sends() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing is bound at this path — the first send() must fail
        // (ENOENT) and disable the sender permanently.
        let path = dir.path().join("nothing-listening.sock");
        let addr = SocketAddr::from_pathname(&path).unwrap();
        let mut sd = SdNotify::from_socket_for_test(&addr);

        assert!(!sd.is_disabled(), "constructed sender starts enabled");
        sd.ready();
        assert!(
            sd.is_disabled(),
            "failed send must disable further attempts"
        );

        // Second send must be a genuine no-op: since `is_disabled()` is
        // already true, `send()`'s early-return path is exercised, not a
        // second real attempt.
        sd.watchdog();
        assert!(sd.is_disabled());
    }

    #[test]
    fn watchdog_interval_from_env_matrix() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let prior = std::env::var("WATCHDOG_USEC").ok();

        // SAFETY-by-convention: single-mutex-guarded, restored below; this
        // is the one function permitted to touch the real env var.
        unsafe {
            std::env::set_var("WATCHDOG_USEC", "60000000");
        }
        assert_eq!(watchdog_interval_from_env(), Some(Duration::from_secs(30)));

        unsafe {
            std::env::set_var("WATCHDOG_USEC", "not-a-number");
        }
        assert_eq!(watchdog_interval_from_env(), None);

        // Negative and u64-overflow values must fall through the same
        // parse-failure path as garbage (reviewer edge pins).
        unsafe {
            std::env::set_var("WATCHDOG_USEC", "-5");
        }
        assert_eq!(watchdog_interval_from_env(), None);

        unsafe {
            std::env::set_var("WATCHDOG_USEC", "99999999999999999999999999");
        }
        assert_eq!(watchdog_interval_from_env(), None);

        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
        }
        assert_eq!(watchdog_interval_from_env(), None);

        match prior {
            Some(v) => unsafe { std::env::set_var("WATCHDOG_USEC", v) },
            None => unsafe { std::env::remove_var("WATCHDOG_USEC") },
        }
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod non_linux_tests {
    use super::SdNotify;

    #[test]
    fn stub_ready_and_watchdog_are_inert() {
        let mut sd = SdNotify::from_env();

        sd.ready();
        sd.watchdog();

        assert_eq!(std::mem::size_of::<SdNotify>(), 0);
    }
}
