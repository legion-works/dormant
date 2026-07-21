//! Thin macOS-only FFI surface backing
//! [`crate::macos_display_sleep::DisplaySleepTransport`]: the `pmset`
//! subprocess call, the raw IOPM user-activity assertion calls, and the raw
//! CoreGraphics per-display asleep readback.
//!
//! Deliberately as small as possible, mirroring
//! `crate::macos_display_catalog`'s "keep it thin" split: every platform-
//! neutral decision (mode validation, timeout wrapping, the poll loop, the
//! RAII release contract) lives in `crate::macos_display_sleep` and is
//! tested there with `FakeDisplaySleepTransport`; this file only ever gets
//! exercised by the macOS CI lane.
//!
//! Reused by Task 11's `doctor` arms for the same IOPM/CoreGraphics
//! primitives (see the plan) — kept as its own module rather than nested
//! inside `macos_display_sleep` for exactly that reuse.
//!
//! DEFERRED: PR CI — this entire module is `#[cfg(target_os = "macos")]`
//! and cannot compile or run in the Linux sandbox this task was implemented
//! in. The IOPM assertion calls (`IOPMAssertionDeclareUserActivity`,
//! `IOPMAssertionRelease`) and the CoreGraphics sleep-state readback
//! (`CGDisplayIsAsleep`) are written to the best of the implementer's
//! knowledge of Apple's `IOPMLib.h`/`CoreGraphics` headers and must be
//! exercised for the first time on the macOS CI lane (Task 2) or real
//! hardware before being trusted — exactly the same caveat
//! `macos_display_catalog.rs` carries for its own FFI surface.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::process::Stdio;

use async_trait::async_trait;
use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8};
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::types::CmdFailure;
use std::ffi::c_int;

use crate::macos_display_sleep::{
    AssertionGuard, CGDirectDisplayID, DisplaySleepTransport, PMSET_SLEEPNOW_ARGS,
};

/// Literal controller name — matches
/// `crate::macos_display_sleep`'s own `NAME` constant (kept as a private
/// literal here too rather than exported, since this module has no other
/// need to import from `macos_display_sleep` beyond the trait/type it
/// implements).
const NAME: &str = "macos-display-sleep";

/// Absolute path to `pmset` — spawned via an argument ARRAY
/// ([`PMSET_SLEEPNOW_ARGS`]), never a shell string.
const PMSET: &str = "/usr/bin/pmset";

/// Maximum stderr bytes surfaced in a `CmdFailure` on non-zero exit —
/// mirrors `crate::kwin_dpms::STDERR_TAIL`.
const STDERR_TAIL: usize = 200;

/// Upper bound on the number of online displays this module will ever
/// enumerate — mirrors `crate::macos_display_catalog::MAX_ONLINE_DISPLAYS`.
const MAX_ONLINE_DISPLAYS: usize = 32;

/// `IOReturn` — `IOKit`'s C error code (`typedef kern_return_t IOReturn`,
/// itself `typedef int kern_return_t`). `0` is `kIOReturnSuccess`.
type IoReturn = i32;
const K_IO_RETURN_SUCCESS: IoReturn = 0;

/// `IOPMAssertionID` (`typedef uint32_t IOPMAssertionID`).
type IoPmAssertionId = u32;

/// `IOPMUserActiveType` — `kIOPMUserActiveLocal = 0`.
type IoPmUserActiveType = u32;
const K_IO_PM_USER_ACTIVE_LOCAL: IoPmUserActiveType = 0;

/// The assertion name string surfaced to `pmset -g assertions` /
/// Activity Monitor's "Prevent sleep" listings while a wake confirmation is
/// in flight — human-diagnostic value only, never parsed back.
const ASSERTION_NAME: &str = "dormant wake confirmation";

#[allow(non_snake_case)]
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        AssertionName: CFStringRef,
        userType: IoPmUserActiveType,
        AssertionID: *mut IoPmAssertionId,
    ) -> IoReturn;

    fn IOPMAssertionRelease(AssertionID: IoPmAssertionId) -> IoReturn;
}

/// `CGError` — Quartz's C error code (`typedef int32_t CGError`). `0` is
/// `kCGErrorSuccess`. Deliberately a distinct alias from [`IoReturn`] even
/// though both are `i32` with `0` as success — they are different C APIs'
/// error domains and comparing/mixing their raw values would be a bug this
/// type distinction exists to catch at review time.
type CgError = i32;
const K_CG_ERROR_SUCCESS: CgError = 0;

#[allow(non_snake_case)]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(
        maxDisplays: u32,
        onlineDisplays: *mut CGDirectDisplayID,
        displayCount: *mut u32,
    ) -> CgError;

    /// SAFETY: `CGDisplayConfiguration.h:288` declares this as returning
    /// Mach `boolean_t`, which is C `int`; nonzero means asleep.
    fn CGDisplayIsAsleep(display: CGDirectDisplayID) -> c_int;
}

/// Build a `CFStringRef` from a Rust `&str` via `CFStringCreateWithCString`
/// (UTF-8). Returns `None` if `s` contains an interior NUL (never true for
/// [`ASSERTION_NAME`], a fixed literal) or Core Foundation itself fails to
/// allocate the string.
///
/// The returned reference is +1-retained per Core Foundation's "Create"
/// naming rule — the caller must `CFRelease` it exactly once.
fn cfstring_from_str(s: &str) -> Option<CFStringRef> {
    let utf8 = CString::new(s).ok()?;
    // Safety: `kCFAllocatorDefault` is a valid allocator constant;
    // `utf8` is a valid, NUL-terminated C string for the duration of
    // this call.
    let cf_string = unsafe {
        CFStringCreateWithCString(kCFAllocatorDefault, utf8.as_ptr(), kCFStringEncodingUTF8)
    };
    if cf_string.is_null() {
        None
    } else {
        Some(cf_string)
    }
}

/// Build a [`CmdFailure`] with the `E_DISPLAY_IO:` prefix and this module's
/// controller name — mirrors
/// `crate::macos_display_sleep::MacosDisplaySleepController::io_err`
/// (private to that module, so this backend has its own single formatting
/// call site instead of exposing that one).
fn io_err(detail: impl std::fmt::Display) -> CmdFailure {
    CmdFailure {
        controller: NAME.to_string(),
        error: format!("{E_DISPLAY_IO}: {detail}"),
    }
}

/// Keep the last `max` characters (best-effort) of `s` as valid UTF-8 —
/// identical algorithm to `crate::kwin_dpms::truncate_utf8_tail` (not
/// shared as a crate-level helper because each module's stderr-truncation
/// call site is its own single, obvious use).
fn truncate_utf8_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let target = s.len().saturating_sub(max);
    let mut idx = target;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    s[idx..].to_string()
}

/// Real IOKit/CoreGraphics/`pmset` backend for
/// [`crate::macos_display_sleep::MacosDisplaySleepController`].
pub struct RealDisplaySleepTransport;

#[async_trait]
impl DisplaySleepTransport for RealDisplaySleepTransport {
    async fn sleep_all(&self) -> Result<(), CmdFailure> {
        // No internal timeout here — `MacosDisplaySleepController::blank`
        // wraps this call with `command_timeout`, mirroring
        // `crate::kwin_dpms::KscreenDoctorTransport::execute`. `kill_on_drop`
        // ensures the OS reclaims the child if the outer timeout fires.
        let child = tokio::process::Command::new(PMSET)
            .args(PMSET_SLEEPNOW_ARGS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| io_err(format!("spawn failed: {e}")))?;

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| io_err(format!("wait failed: {e}")))?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail = truncate_utf8_tail(&stderr, STDERR_TAIL);
            let detail = match output.status.code() {
                Some(c) => format!("exit code {c}; stderr: {tail}"),
                None => format!("terminated by signal; stderr: {tail}"),
            };
            Err(io_err(detail))
        }
    }

    fn declare_user_activity(&self) -> Result<AssertionGuard, CmdFailure> {
        let Some(name_ref) = cfstring_from_str(ASSERTION_NAME) else {
            return Err(io_err("failed to build the assertion name CFString"));
        };

        let mut assertion_id: IoPmAssertionId = 0;
        // Safety: `name_ref` was just checked non-null and is a valid,
        // owned CFStringRef; `assertion_id` is a valid out-param.
        let err = unsafe {
            IOPMAssertionDeclareUserActivity(
                name_ref,
                K_IO_PM_USER_ACTIVE_LOCAL,
                &raw mut assertion_id,
            )
        };
        // Safety: `name_ref` is a +1-retained reference owned by this
        // function; release it exactly once, after the call that consumes
        // it (IOPMAssertionDeclareUserActivity does not take ownership).
        unsafe { core_foundation_sys::base::CFRelease(name_ref.cast()) };

        if err != K_IO_RETURN_SUCCESS {
            return Err(io_err(format!(
                "IOPMAssertionDeclareUserActivity failed: IOReturn {err}"
            )));
        }

        Ok(AssertionGuard::new(move || {
            // Safety: `assertion_id` was returned by a successful
            // `IOPMAssertionDeclareUserActivity` call above and has not
            // been released before (this closure runs at most once — see
            // `AssertionGuard::drop`).
            let release_err = unsafe { IOPMAssertionRelease(assertion_id) };
            if release_err != K_IO_RETURN_SUCCESS {
                tracing::warn!(
                    event = "macos_display_sleep_assertion_release_failed",
                    assertion_id,
                    error = release_err,
                    "IOPMAssertionRelease failed — best-effort, nothing further to do",
                );
            }
        }))
    }

    fn online_sleep_states(&self) -> Result<Vec<(CGDirectDisplayID, bool)>, CmdFailure> {
        let mut ids = [0u32; MAX_ONLINE_DISPLAYS];
        let mut count: u32 = 0;
        let max_displays = u32::try_from(MAX_ONLINE_DISPLAYS)
            .map_err(|_| io_err("online display limit does not fit in u32"))?;
        // Safety: `ids` has `MAX_ONLINE_DISPLAYS` capacity, matching
        // `maxDisplays`; `count` is a valid out-param.
        let err = unsafe { CGGetOnlineDisplayList(max_displays, ids.as_mut_ptr(), &raw mut count) };
        if err != K_CG_ERROR_SUCCESS {
            return Err(io_err(format!(
                "CGGetOnlineDisplayList failed: CGError {err}"
            )));
        }

        let mut out = Vec::with_capacity(count as usize);
        for &id in &ids[..count as usize] {
            // Safety: `id` came from the just-populated online display list.
            let asleep = unsafe { CGDisplayIsAsleep(id) } != 0;
            out.push((id, asleep));
        }
        Ok(out)
    }
}
