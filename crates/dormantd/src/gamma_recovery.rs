//! macOS gamma-continuity recovery: startup and shutdown handling for the
//! `gamma-blank.json` breadcrumb written by
//! `dormant_displays::gamma_breadcrumb::GammaBreadcrumb` (see that module
//! for the on-disk shape and the add/remove-selector contract
//! `dormant_displays::macos_gamma_black::MacosGammaBlackController` drives).
//!
//! ## Two triggers, one shared function
//!
//! - **Startup** (`main.rs`, before ANY config load, `boot_guard::prepare`,
//!   `App::build`, or assembly): a breadcrumb surviving from an unclean
//!   prior exit means the panel(s) it names are still driving an all-zero
//!   gamma table with nothing left alive that remembers how to restore
//!   them. The daemon restores the system-wide default color/gamma state
//!   via [`GammaSystemRestore::restore_all`] before doing anything else
//!   that could fail, delay, or (on an invalid config) get skipped.
//! - **Shutdown** (`main.rs`'s `run_to_completion`, around `run_loop`'s
//!   return — normal exit, SIGTERM, or SIGINT): a still-present breadcrumb
//!   at process-exit time means at least one gamma selector's hold was
//!   never cleared by a confirmed wake. Same best-effort restore.
//!
//! Both call the SAME [`restore_stale_breadcrumb`] — there is exactly one
//! restore-then-clear sequence in this module, invoked from two call sites
//! in `main.rs`, not two parallel implementations.
//!
//! **SIGHUP (config reload) NEVER reaches this module.** Reload continuity
//! is `dormant_displays::registry::ControllerBuildContext` reusing the
//! SAME [`dormant_displays::macos_gamma_black::GammaHoldRegistry`] and
//! [`dormant_displays::gamma_breadcrumb::GammaBreadcrumb`] across every
//! generation (constructed once in `App::start`, see that function's
//! docs) — a gamma-blanked display survives a reload by nobody touching
//! it, not by a restore-and-reacquire cycle. `crate::app::Runner::reload`
//! and `crate::app::Runner::rebuild_old` never call
//! [`GammaSystemRestore`] anywhere in their bodies (grep-verified).
//!
//! ## Failure handling
//!
//! Restoration failure is LOGGED, never propagated as an error — this
//! module must never convert a self-restoring crash-recovery path into a
//! startup abort or a shutdown hang. Every function here returns `()`.

use std::path::Path;

use dormant_displays::gamma_breadcrumb::GammaBreadcrumb;

/// Abstract system-wide gamma/`ColorSync` restore call — real
/// (`CGDisplayRestoreColorSyncSettings`, macOS-cfg'd, see
/// [`RealGammaSystemRestore`]) or fake (tests). Narrow and synchronous like
/// `dormant_displays::macos_gamma_black::GammaApi` — a local, in-process
/// Quartz call, no bus/network I/O.
pub trait GammaSystemRestore: Send + Sync {
    /// Best-effort, idempotent: restore every display's gamma/`ColorSync`
    /// state to the system default. Must not panic; returns an error
    /// string for the caller to log — never propagated further than a log
    /// line (see module docs).
    ///
    /// # Errors
    ///
    /// Returns a description of the failure; callers always treat this as
    /// non-fatal.
    fn restore_all(&self) -> Result<(), String>;
}

/// Real backend. On macOS this calls `CGDisplayRestoreColorSyncSettings()`
/// (declared locally — this is the only call site in the codebase that
/// needs it, so it doesn't earn a place in
/// `dormant_displays::macos_display_catalog`'s thin FFI surface). Off
/// macOS this is a no-op: there is no Quartz gamma API to restore, but the
/// breadcrumb-check / restore-then-clear ORDERING logic around it still
/// runs identically so it stays Linux-testable (see module docs).
///
/// DEFERRED: PR CI — the macOS arm below cannot compile or run in this
/// Linux sandbox; it must be exercised for the first time on the macOS CI
/// lane (Task 2) or real hardware before being trusted, per the same
/// caveat `dormant_displays::macos_display_catalog`'s module doc carries
/// for its own FFI surface.
pub struct RealGammaSystemRestore;

#[cfg(target_os = "macos")]
mod ffi {
    #[allow(non_snake_case)]
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        /// `void CGDisplayRestoreColorSyncSettings(void)` — Apple's header
        /// declares no return value and no documented failure mode; it is
        /// the coarse, system-wide "put every display's ColorSync/gamma
        /// state back to what the current profile says it should be" call,
        /// the deliberately blunt instrument this module reaches for once
        /// a per-selector saved-table replay is no longer possible (the
        /// in-process `GammaHoldRegistry` that held it is gone).
        pub(super) fn CGDisplayRestoreColorSyncSettings();
    }
}

#[cfg(target_os = "macos")]
impl GammaSystemRestore for RealGammaSystemRestore {
    fn restore_all(&self) -> Result<(), String> {
        // Safety: `CGDisplayRestoreColorSyncSettings` takes no arguments,
        // returns `void`, and per Apple's documentation is safe to call at
        // any time (it is the same call System Preferences/Settings itself
        // issues when a user resets ColorSync).
        unsafe { ffi::CGDisplayRestoreColorSyncSettings() };
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
impl GammaSystemRestore for RealGammaSystemRestore {
    fn restore_all(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Startup/shutdown breadcrumb check (see module docs): if `state_dir`'s
/// breadcrumb exists, call `restore.restore_all()` and clear the marker on
/// success. Always logs its outcome; never returns an error — this must
/// never abort startup or hang shutdown. A no-op (no log at all) when no
/// breadcrumb exists.
///
/// `caller` is a short label (`"startup"` / `"shutdown"`) folded into the
/// log event names so the two call sites remain distinguishable in
/// operator logs despite sharing this one implementation.
pub fn restore_stale_breadcrumb(state_dir: &Path, restore: &dyn GammaSystemRestore, caller: &str) {
    let breadcrumb = GammaBreadcrumb::new(state_dir);
    if !breadcrumb.exists() {
        return;
    }
    match restore.restore_all() {
        Ok(()) => match breadcrumb.delete() {
            Ok(()) => {
                tracing::info!(event = "gamma_stale_breadcrumb_restored", caller = %caller);
            }
            Err(e) => {
                tracing::warn!(
                    event = "gamma_breadcrumb_clear_failed",
                    caller = %caller,
                    error = %e,
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                event = "gamma_stale_breadcrumb_restore_failed",
                caller = %caller,
                error = %e,
            );
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct FakeRestore {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    impl FakeRestore {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl GammaSystemRestore for FakeRestore {
        fn restore_all(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err("simulated restore failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// No breadcrumb present: `restore_all` must never be called.
    #[test]
    fn no_breadcrumb_never_calls_restore() {
        let dir = temp_dir();
        let restore = FakeRestore::default();
        restore_stale_breadcrumb(dir.path(), &restore, "startup");
        assert_eq!(restore.calls(), 0);
    }

    /// RED-first test 7 (Task 8 plan):
    /// `stale_breadcrumb_restores_before_assembly_and_then_clears` — marker
    /// written, restore succeeds → restore called once, file gone. This
    /// function takes no config/assembly dependency at all (only
    /// `state_dir` + the restore seam) — see the module docs' "Two
    /// triggers, one shared function" section and this task's report for
    /// why that constructor shape is itself the structural proof that
    /// nothing about config-load/assembly ordering can suppress this call
    /// when wired at the top of `main()`, ahead of any config-touching
    /// call.
    #[test]
    fn stale_breadcrumb_restores_before_assembly_and_then_clears() {
        let dir = temp_dir();
        let breadcrumb = GammaBreadcrumb::new(dir.path());
        breadcrumb.add_selector("cg:panel").unwrap();
        assert!(breadcrumb.exists());

        let restore = FakeRestore::default();
        restore_stale_breadcrumb(dir.path(), &restore, "startup");

        assert_eq!(restore.calls(), 1);
        assert!(
            !breadcrumb.exists(),
            "breadcrumb must be cleared after a successful restore"
        );
    }

    /// A failed restore must NOT clear the breadcrumb (so the next
    /// boot/shutdown check retries) and must not panic.
    #[test]
    fn failed_restore_keeps_breadcrumb_for_a_retry() {
        let dir = temp_dir();
        let breadcrumb = GammaBreadcrumb::new(dir.path());
        breadcrumb.add_selector("cg:panel").unwrap();

        let restore = FakeRestore {
            fail: true,
            ..Default::default()
        };
        restore_stale_breadcrumb(dir.path(), &restore, "startup");

        assert_eq!(restore.calls(), 1);
        assert!(
            breadcrumb.exists(),
            "a failed restore must leave the breadcrumb in place for the next check"
        );
    }

    /// Two selectors held: one restore call clears the WHOLE breadcrumb
    /// (the restore is system-wide, not per-selector — see module docs).
    #[test]
    fn multiple_held_selectors_are_cleared_by_one_restore_call() {
        let dir = temp_dir();
        let breadcrumb = GammaBreadcrumb::new(dir.path());
        breadcrumb.add_selector("cg:a").unwrap();
        breadcrumb.add_selector("cg:b").unwrap();

        let restore = FakeRestore::default();
        restore_stale_breadcrumb(dir.path(), &restore, "shutdown");

        assert_eq!(restore.calls(), 1);
        assert!(!breadcrumb.exists());
    }

    /// Real backend must not panic when constructed/called on this (Linux)
    /// target — the no-op arm. Also documents that off-macOS,
    /// `RealGammaSystemRestore` always succeeds trivially, per module docs.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn real_backend_is_a_harmless_noop_off_macos() {
        let real = RealGammaSystemRestore;
        assert!(real.restore_all().is_ok());
    }
}
