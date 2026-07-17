//! Thin macOS-only FFI surface backing [`crate::macos_gamma_black::GammaApi`]:
//! stable-UUID display resolution and the raw Quartz gamma-table calls.
//!
//! This module is deliberately as small as possible — every extern
//! declaration here is the minimum needed to implement
//! [`crate::macos_gamma_black::GammaApi`]'s three methods, per the plan's
//! "keep it thin" instruction (Task 6/7 both draw this line: the platform-
//! neutral controller logic lives in `macos_gamma_black.rs` and is tested
//! there with [`crate::macos_gamma_black`]'s `FakeGammaApi`; this file only
//! ever gets exercised by the macOS CI lane).
//!
//! DEFERRED: PR CI — this entire module is `#[cfg(target_os = "macos")]`
//! and cannot compile or run in the Linux sandbox this task was implemented
//! in. It was written to the best of the implementer's knowledge of the
//! `CoreGraphics` gamma-table API (`CGGetOnlineDisplayList`,
//! `CGDisplayCreateUUIDFromDisplayID`, `CGDisplayGammaTableCapacity`,
//! `CGGetDisplayTransferByTable`, `CGSetDisplayTransferByTable`) and must be
//! exercised for the first time on the macOS CI lane (Task 2) or real
//! hardware before being trusted.

#![cfg(target_os = "macos")]

use core_foundation_sys::base::{CFRelease, kCFAllocatorDefault};
use core_foundation_sys::string::{CFStringGetCString, CFStringRef, kCFStringEncodingUTF8};
use core_foundation_sys::uuid::{CFUUIDCreateString, CFUUIDRef};

use crate::macos_gamma_black::{CGDirectDisplayID, GammaApi, GammaError, GammaTable};

/// `CGError` — Quartz's C error code (`typedef int32_t CGError`).
/// `0` is `kCGErrorSuccess`.
type CGError = i32;

/// Upper bound on the number of online displays this module will ever
/// enumerate. Generous for any real desk setup; a hard cap keeps
/// [`RealGammaApi::resolve`]'s stack buffer a fixed, small size.
const MAX_ONLINE_DISPLAYS: usize = 32;

#[allow(non_snake_case)]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(
        maxDisplays: u32,
        onlineDisplays: *mut CGDirectDisplayID,
        displayCount: *mut u32,
    ) -> CGError;

    fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> CFUUIDRef;

    fn CGDisplayGammaTableCapacity(display: CGDirectDisplayID) -> u32;

    fn CGGetDisplayTransferByTable(
        display: CGDirectDisplayID,
        capacity: u32,
        redTable: *mut f32,
        greenTable: *mut f32,
        blueTable: *mut f32,
        sampleCount: *mut u32,
    ) -> CGError;

    fn CGSetDisplayTransferByTable(
        display: CGDirectDisplayID,
        tableSize: u32,
        redTable: *const f32,
        greenTable: *const f32,
        blueTable: *const f32,
    ) -> CGError;
}

/// Convert a `CGDirectDisplayID`'s stable `CFUUID` (via
/// `CGDisplayCreateUUIDFromDisplayID`) into the `cg:<lowercase-uuid>`
/// selector string this controller family uses (Task 4 ratified contract).
///
/// Returns `None` when the display has no associated UUID (rare — e.g. a
/// virtual/headless display) or the UUID cannot be stringified.
fn selector_for_display(display: CGDirectDisplayID) -> Option<String> {
    // Safety: `display` is a live `CGDirectDisplayID` obtained from
    // `CGGetOnlineDisplayList` immediately before this call; the returned
    // `CFUUIDRef` is a +1-retained reference per Core Foundation's "Create"
    // naming rule, released below.
    let uuid_ref: CFUUIDRef = unsafe { CGDisplayCreateUUIDFromDisplayID(display) };
    if uuid_ref.is_null() {
        return None;
    }

    // Safety: `uuid_ref` was just checked non-null and is a valid, owned
    // CFUUIDRef; `CFUUIDCreateString` returns a +1-retained CFStringRef.
    let cf_string: CFStringRef = unsafe { CFUUIDCreateString(kCFAllocatorDefault, uuid_ref) };
    let result = if cf_string.is_null() {
        None
    } else {
        let mut buf = [0i8; 64];
        // Safety: `cf_string` is non-null and valid; `buf` is large enough
        // for any CFUUID string representation
        // ("XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX" is 36 bytes + NUL).
        let ok = unsafe {
            CFStringGetCString(
                cf_string,
                buf.as_mut_ptr(),
                buf.len() as isize,
                kCFStringEncodingUTF8,
            )
        };
        let s = if ok != 0 {
            // Safety: `buf` was just filled by a successful
            // `CFStringGetCString`, which NUL-terminates on success.
            let c_str = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
            c_str.to_str().ok().map(str::to_ascii_lowercase)
        } else {
            None
        };
        // Safety: `cf_string` is a +1-retained reference owned by this
        // function; release it exactly once, after the last use above.
        unsafe { CFRelease(cf_string.cast()) };
        s
    };

    // Safety: `uuid_ref` is a +1-retained reference owned by this function;
    // release it exactly once, after the last use above.
    unsafe { CFRelease(uuid_ref.cast()) };

    result.map(|uuid| format!("cg:{uuid}"))
}

/// Real macOS Quartz gamma-table backend for
/// [`crate::macos_gamma_black::MacosGammaBlackController`].
pub struct RealGammaApi;

impl GammaApi for RealGammaApi {
    fn resolve(&self, selector: &str) -> Result<CGDirectDisplayID, GammaError> {
        let mut ids = [0u32; MAX_ONLINE_DISPLAYS];
        let mut count: u32 = 0;
        // Safety: `ids` has `MAX_ONLINE_DISPLAYS` capacity, matching
        // `maxDisplays`; `count` is a valid out-param.
        let err = unsafe {
            CGGetOnlineDisplayList(MAX_ONLINE_DISPLAYS as u32, ids.as_mut_ptr(), &mut count)
        };
        if err != 0 {
            return Err(GammaError::from(format!(
                "CGGetOnlineDisplayList failed: CGError {err}"
            )));
        }

        for &id in &ids[..count as usize] {
            if selector_for_display(id).as_deref() == Some(selector) {
                return Ok(id);
            }
        }

        Err(GammaError::from(format!(
            "no online display matches selector '{selector}'"
        )))
    }

    fn read_table(&self, display: CGDirectDisplayID) -> Result<GammaTable, GammaError> {
        // Safety: `display` is caller-provided; a stale/invalid ID is a
        // normal Quartz error return, not undefined behavior.
        let capacity = unsafe { CGDisplayGammaTableCapacity(display) };
        if capacity == 0 {
            return Err(GammaError::from(format!(
                "display {display} reports a zero-capacity gamma table"
            )));
        }

        let mut red = vec![0f32; capacity as usize];
        let mut green = vec![0f32; capacity as usize];
        let mut blue = vec![0f32; capacity as usize];
        let mut sample_count: u32 = 0;

        // Safety: the three buffers each have `capacity` elements, matching
        // the `capacity` argument; `sample_count` is a valid out-param.
        let err = unsafe {
            CGGetDisplayTransferByTable(
                display,
                capacity,
                red.as_mut_ptr(),
                green.as_mut_ptr(),
                blue.as_mut_ptr(),
                &mut sample_count,
            )
        };
        if err != 0 {
            return Err(GammaError::from(format!(
                "CGGetDisplayTransferByTable failed for display {display}: CGError {err}"
            )));
        }

        red.truncate(sample_count as usize);
        green.truncate(sample_count as usize);
        blue.truncate(sample_count as usize);
        let table = GammaTable { red, green, blue };
        table
            .validate()
            .map_err(|e| GammaError::from(format!("readback produced an invalid table: {e}")))?;
        Ok(table)
    }

    fn write_table(
        &self,
        display: CGDirectDisplayID,
        table: &GammaTable,
    ) -> Result<(), GammaError> {
        table
            .validate()
            .map_err(|e| GammaError::from(format!("refusing to write an invalid table: {e}")))?;

        let size = u32::try_from(table.len())
            .map_err(|_| GammaError::from("gamma table length does not fit in u32"))?;

        // Safety: `table.red/green/blue` each have exactly `table.len()`
        // elements (validated above), matching `tableSize`.
        let err = unsafe {
            CGSetDisplayTransferByTable(
                display,
                size,
                table.red.as_ptr(),
                table.green.as_ptr(),
                table.blue.as_ptr(),
            )
        };
        if err != 0 {
            return Err(GammaError::from(format!(
                "CGSetDisplayTransferByTable failed for display {display}: CGError {err}"
            )));
        }
        Ok(())
    }
}
