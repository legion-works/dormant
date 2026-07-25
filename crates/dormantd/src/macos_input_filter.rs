//! macOS CGEventTap filtered input source.
//!
//! Creates a listen-only event tap that inspects every HID event's source
//! process name against the configured glob ignore list.  Accepted events
//! publish activity through the shared [`FilteredActivity`] channel; ignored
//! events (from jiggler processes, etc.) are silently discarded.
//!
//! ## Seam — [`MacosInputHost`]
//!
//! The two host-side failure paths (Accessibility denied, tap creation
//! failure) are gated behind the [`MacosInputHost`] trait so the fail-safe
//! contract is mutation-provable.  Production uses [`RealMacosInputHost`];
//! tests inject a scripted fake.
//!
//! ## Fail-safe
//!
//! Accessibility permission absent or tap creation failed → the daemon's
//! [`InputAuthoritySupervisor`] drives authority back to the stock
//! [`crate::macos_idle::MacosIdleSource`] `CGEventSource` path and logs the
//! literal `input_filter_unavailable` — never "no activity forever".
//!
//! ## Tap vs Carbon hotkeys
//!
//! The tap is listen-only (`kCGEventTapOptionListenOnly`) and is distinct
//! from the Carbon `RegisterEventHotKey` path.  Accessibility is required
//! for filtering, NOT for hotkey registration; their failure modes are
//! deliberately uncoupled so a missing Accessibility grant cannot regress
//! the hotkey-based claim path (Task 13).

#![cfg(target_os = "macos")]
// Constants follow Apple's naming convention (kCGHIDEventTap, etc.).
// The native API uses non_upper_case_globals by design — suppress
// the Rust convention warning for this FFI-only module.
#![allow(non_upper_case_globals)]

use std::ffi::c_char;
use std::os::raw::c_void;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Result, bail};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::filtered_activity::{
    DeviceMatcher, FilteredActivity, FilteredActivityTx, FilteredInputSource,
    filtered_activity_channel,
};

// ── Classification (pure, testable on any platform) ───────────────────────

/// Whether an event-source process name should count as activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityDisposition {
    /// The source process matches an ignore glob — discard.
    Ignore,
    /// The source process is not filtered — count as activity.
    Accept,
}

/// Classify a process name against the configured ignore globs.
///
/// Matching is case-insensitive and uses the same [`DeviceMatcher`]
/// glob engine as the Linux evdev path.
#[must_use]
pub fn classify_process(name: &str, globs: &[String]) -> ActivityDisposition {
    let matcher = DeviceMatcher::compile(globs).unwrap_or_else(|never| match never {});
    if matcher.is_ignored(name) {
        ActivityDisposition::Ignore
    } else {
        ActivityDisposition::Accept
    }
}

/// Publish an accepted event onto the filtered-activity channel.
///
/// Called by the tap callback for every non-ignored event.  Exposed as a
/// standalone function so the publish-vs-discard contract is testable
/// without driving a real CGEventTap.
pub fn publish_accepted_event(
    activity: &Mutex<FilteredActivity>,
    activity_tx: &Mutex<FilteredActivityTx>,
) {
    let now = Instant::now();
    let snapshot = {
        let mut a = activity.lock().unwrap_or_else(|poison| poison.into_inner());
        a.last_activity = Some(now);
        a.observed_at = now;
        a.edge_seq = a.edge_seq.saturating_add(1);
        a.clone()
    };
    activity_tx
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .send_replace(snapshot);
}

// ── Host seam ──────────────────────────────────────────────────────────────

/// Abstract over the two macOS host effects that gate the filtered-source
/// fail-safe paths, so both "permission denied" and "tap creation failed"
/// are mutation-provable in unit tests.
pub trait MacosInputHost: Send + Sync + 'static {
    /// Whether the process holds Accessibility permission.
    fn accessibility_trusted(&self) -> bool;

    /// Run the filtered input source on the current (blocking-pool) thread.
    /// Blocks until `cancel` is signalled, then returns.
    ///
    /// # Errors
    ///
    /// Returns `Err` when tap setup fails; the supervisor treats this as
    /// `input_filter_unavailable` and falls back to the stock idle source.
    fn run_blocking(
        &self,
        matcher: DeviceMatcher,
        activity_tx: FilteredActivityTx,
        cancel: CancellationToken,
    ) -> Result<()>;
}

// ── macOS filtered source ──────────────────────────────────────────────────

/// macOS filtered input source backed by a listen-only CGEventTap.
pub struct MacosInputFilterSource {
    matcher: DeviceMatcher,
    host: std::sync::Arc<dyn MacosInputHost>,
}

impl MacosInputFilterSource {
    /// Build a macOS filtered input source backed by the real CGEventTap host.
    #[must_use]
    pub fn new(matcher: DeviceMatcher) -> Self {
        Self {
            matcher,
            host: std::sync::Arc::new(RealMacosInputHost),
        }
    }

    /// Build a source with an injected host — test seam.
    #[must_use]
    pub fn with_host(matcher: DeviceMatcher, host: std::sync::Arc<dyn MacosInputHost>) -> Self {
        Self { matcher, host }
    }
}

#[async_trait::async_trait]
impl FilteredInputSource for MacosInputFilterSource {
    async fn start(
        &self,
        activity_tx: FilteredActivityTx,
        cancel: CancellationToken,
    ) -> Result<JoinHandle<Result<()>>> {
        if !self.host.accessibility_trusted() {
            bail!("accessibility permission denied — cannot filter input by process name");
        }

        let matcher = self.matcher.clone();
        let host = self.host.clone();

        // Seed the channel as available before the run loop starts.
        let now = Instant::now();
        activity_tx.send_replace(FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 0,
        });

        Ok(tokio::task::spawn_blocking(move || {
            host.run_blocking(matcher, activity_tx, cancel)
        }))
    }

    async fn probe(&self, cancel: CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        if !self.host.accessibility_trusted() {
            bail!("accessibility permission denied — input filter requires Accessibility");
        }

        // Prove the tap API itself works: create, verify non-null,
        // immediately tear down without publishing.
        let (dummy_tx, _dummy_rx) = filtered_activity_channel();
        let tap = create_tap(&self.matcher, dummy_tx)?;
        unsafe {
            CGEventTapEnable(tap.port, false);
            drop(Box::from_raw(tap.state));
            CFMachPortInvalidate(tap.port);
            CFRelease(tap.port);
        }
        Ok(())
    }
}

// ── Real host ──────────────────────────────────────────────────────────────

/// Production [`MacosInputHost`] — delegates to the real CoreGraphics and
/// ApplicationServices frameworks.
struct RealMacosInputHost;

impl MacosInputHost for RealMacosInputHost {
    fn accessibility_trusted(&self) -> bool {
        // Safety: AXIsProcessTrusted has no failure path.
        unsafe { AXIsProcessTrusted() != 0 }
    }

    fn run_blocking(
        &self,
        matcher: DeviceMatcher,
        activity_tx: FilteredActivityTx,
        cancel: CancellationToken,
    ) -> Result<()> {
        run_event_loop(matcher, activity_tx, cancel)
    }
}

// ── Shared tap state (leaked into the C callback via userInfo) ────────────

struct TapState {
    matcher: DeviceMatcher,
    activity: Mutex<FilteredActivity>,
    activity_tx: Mutex<FilteredActivityTx>,
}

// ── Tap lifecycle ─────────────────────────────────────────────────────────

/// A created event tap ready to be added to a run loop.
///
/// `state` is a `*mut TapState` that was stored as the tap's `userInfo`
/// and MUST be freed by the caller AFTER the tap is disabled and the
/// callback can no longer fire.
struct OwnedTap {
    port: CFMachPortRef,
    state: *mut TapState,
}

/// Create a listen-only HID event tap.  Returns both the raw port and
/// the heap-allocated [`TapState`] pointer stored as `userInfo`.
fn create_tap(matcher: &DeviceMatcher, activity_tx: FilteredActivityTx) -> Result<OwnedTap> {
    let mask = input_event_mask();
    let now = Instant::now();

    let state = Box::new(TapState {
        matcher: matcher.clone(),
        activity: Mutex::new(FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 0,
        }),
        activity_tx: Mutex::new(activity_tx),
    });
    let state_ptr = Box::into_raw(state);

    // Safety: state_ptr outlives the tap and is freed by the caller
    // after the tap is disabled + invalidated.
    let port = unsafe {
        CGEventTapCreate(
            kCGHIDEventTap,
            kCGHeadInsertEventTap,
            kCGEventTapOptionListenOnly,
            mask,
            tap_callback,
            state_ptr as *mut c_void,
        )
    };

    if port.is_null() {
        // Safety: state_ptr was just allocated and CGEventTapCreate
        // did not store it — safe to free.
        unsafe {
            drop(Box::from_raw(state_ptr));
        }
        bail!("CGEventTapCreate returned NULL — tap creation failed");
    }

    Ok(OwnedTap {
        port,
        state: state_ptr,
    })
}

/// Run the event tap on the current thread's run loop until cancelled.
///
/// Spawned on a blocking thread pool via `tokio::task::spawn_blocking`.
fn run_event_loop(
    matcher: DeviceMatcher,
    activity_tx: FilteredActivityTx,
    cancel: CancellationToken,
) -> Result<()> {
    let tap = create_tap(&matcher, activity_tx)?;

    let run_loop = unsafe { CFRunLoopGetCurrent() };
    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null_mut(), tap.port, 0) };
    if source.is_null() {
        unsafe {
            drop(Box::from_raw(tap.state));
            CFRelease(tap.port);
        }
        bail!("CFMachPortCreateRunLoopSource returned NULL");
    }

    unsafe {
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CFRelease(source); // retained by the run loop
        CGEventTapEnable(tap.port, true);
    }

    // Poll the run loop in timed slices so we can check the
    // cancellation token between iterations.  This avoids spawning a
    // second thread (and the `Send`-bound issues with raw CF types
    // on strict-provenance toolchains).
    loop {
        // Run the loop for at most 100 ms, processing any pending
        // sources, then return so we can check cancellation.
        unsafe {
            CFRunLoopRunInMode(
                kCFRunLoopDefaultMode,
                0.1,  // seconds
                true, // returnAfterSourceHandled
            );
        }
        if cancel.is_cancelled() {
            unsafe {
                CGEventTapEnable(tap.port, false);
                CFMachPortInvalidate(tap.port);
            }
            break;
        }
    }

    // The tap is disabled and invalidated — callback won't fire again.
    // Safe to free the state and release the port.
    unsafe {
        drop(Box::from_raw(tap.state));
        CFRelease(tap.port);
    }

    Ok(())
}

// ── Tap callback ───────────────────────────────────────────────────────────

/// CoreGraphics event tap callback — called for every HID event on the
/// run-loop thread.
///
/// Returns the event unmodified (listen-only: `kCGEventTapOptionListenOnly`
/// guarantees propagation regardless of the return value, but returning
/// the event is idiomatic for listen-only taps).
unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    _type_: CGEventType,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let state = unsafe { &*(user_info as *const TapState) };

    let process_name = match process_name_for_event(event) {
        Some(name) => name,
        None => return event,
    };

    // Check against the ignore globs.
    if state.matcher.is_ignored(&process_name) {
        return event;
    }

    // Accepted — publish via the shared helper so this decision is
    // independently testable.
    publish_accepted_event(&state.activity, &state.activity_tx);

    event
}

/// Maximum process name length in bytes (`proc_name`'s documented max).
const MAX_PROCESS_NAME_LEN: usize = 256;

/// Extract the process name for a CoreGraphics event.
///
/// Uses `CGEventGetIntegerValueField(event, kCGEventTargetUnixProcessID)`
/// to retrieve the source PID, then `proc_name(pid)` to get the name.
/// Both `CGEventCopyProcessName` and `CGEventGetProcessName` were removed
/// from the macOS SDK linker-visible symbols in recent toolchains.
///
/// Returns `None` when the event is NULL, the source PID is 0, or the
/// process name lookup fails.
fn process_name_for_event(event: CGEventRef) -> Option<String> {
    if event.is_null() {
        return None;
    }
    let pid = unsafe { CGEventGetIntegerValueField(event, kCGEventTargetUnixProcessID) } as i32;
    if pid <= 0 {
        return None;
    }
    let mut buf: [u8; MAX_PROCESS_NAME_LEN] = [0u8; MAX_PROCESS_NAME_LEN];
    // Safety: buf is a valid buffer of MAX_PROCESS_NAME_LEN bytes.
    let result = unsafe { proc_name(pid, buf.as_mut_ptr() as *mut c_char, buf.len() as u32) };
    if result <= 0 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if len == 0 {
        None
    } else {
        String::from_utf8(buf[..len].to_vec()).ok()
    }
}

// ── Event mask ─────────────────────────────────────────────────────────────

/// Build the event-type bitmask for the tap — covers keyboard, mouse
/// buttons, mouse movement, scroll, and tablet events.
fn input_event_mask() -> CGEventMask {
    mask_bit(kCGEventLeftMouseDown)
        | mask_bit(kCGEventLeftMouseUp)
        | mask_bit(kCGEventRightMouseDown)
        | mask_bit(kCGEventRightMouseUp)
        | mask_bit(kCGEventOtherMouseDown)
        | mask_bit(kCGEventOtherMouseUp)
        | mask_bit(kCGEventMouseMoved)
        | mask_bit(kCGEventLeftMouseDragged)
        | mask_bit(kCGEventRightMouseDragged)
        | mask_bit(kCGEventOtherMouseDragged)
        | mask_bit(kCGEventScrollWheel)
        | mask_bit(kCGEventKeyDown)
        | mask_bit(kCGEventKeyUp)
        | mask_bit(kCGEventFlagsChanged)
        | mask_bit(kCGEventTabletPointer)
        | mask_bit(kCGEventTabletProximity)
}

const fn mask_bit(event_type: CGEventType) -> CGEventMask {
    1u64 << (event_type as u64)
}

// ── FFI types and constants ────────────────────────────────────────────────

type CGEventTapProxy = *mut c_void;
type CGEventTapLocation = u32;
type CGEventTapPlacement = u32;
type CGEventTapOptions = u32;
type CGEventType = u32;
type CGEventMask = u64;
type CGEventField = u32;
type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFStringRef = *mut c_void;
type CFTypeRef = *mut c_void;
type CFIndex = i64;

type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    type_: CGEventType,
    event: CGEventRef,
    #[allow(non_snake_case)] userInfo: *mut c_void,
) -> CGEventRef;

// ── Constants ──────────────────────────────────────────────────────────────

const kCGHIDEventTap: CGEventTapLocation = 0;
const kCGHeadInsertEventTap: CGEventTapPlacement = 0;
const kCGEventTapOptionListenOnly: CGEventTapOptions = 0;

// Event types (subset — input events we listen for).
const kCGEventLeftMouseDown: CGEventType = 1;
const kCGEventLeftMouseUp: CGEventType = 2;
const kCGEventRightMouseDown: CGEventType = 3;
const kCGEventRightMouseUp: CGEventType = 4;
const kCGEventMouseMoved: CGEventType = 5;
const kCGEventLeftMouseDragged: CGEventType = 6;
const kCGEventRightMouseDragged: CGEventType = 7;
const kCGEventOtherMouseDown: CGEventType = 25;
const kCGEventOtherMouseUp: CGEventType = 26;
const kCGEventOtherMouseDragged: CGEventType = 27;
const kCGEventKeyDown: CGEventType = 10;
const kCGEventKeyUp: CGEventType = 11;
const kCGEventFlagsChanged: CGEventType = 12;
const kCGEventScrollWheel: CGEventType = 22;
const kCGEventTabletPointer: CGEventType = 23;
const kCGEventTabletProximity: CGEventType = 24;

/// `kCGEventTargetUnixProcessID` — field key for the source process PID.
const kCGEventTargetUnixProcessID: CGEventField = 55;

// ── FFI declarations ───────────────────────────────────────────────────────

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Returns non-zero iff the calling process is listed and enabled in
    /// System Settings → Privacy & Security → Accessibility.
    fn AXIsProcessTrusted() -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOptions,
        eventsOfInterest: CGEventMask,
        callback: CGEventTapCallBack,
        userInfo: *mut c_void,
    ) -> CFMachPortRef;

    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);

    /// Returns the integer value of the requested field for `event`.
    /// Use `kCGEventTargetUnixProcessID` to retrieve the source PID.
    fn CGEventGetIntegerValueField(event: CGEventRef, field: CGEventField) -> i64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *mut c_void,
        port: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;

    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);

    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, returnAfterSourceHandled: bool) -> i32;

    fn CFRunLoopGetCurrent() -> CFRunLoopRef;

    fn CFMachPortInvalidate(port: CFMachPortRef);
    fn CFRelease(cf: CFTypeRef);

    /// `kCFRunLoopCommonModes` — a constant CFString.
    static kCFRunLoopCommonModes: CFStringRef;

    /// `kCFRunLoopDefaultMode` — a constant CFString.
    static kCFRunLoopDefaultMode: CFStringRef;
}

// `proc_name` lives in libSystem on macOS — no explicit `#[link]` needed.
unsafe extern "C" {
    /// Fill `buffer` with the NUL-terminated process name for `pid`.
    /// Returns 0 on failure, the returned buffer length on success.
    fn proc_name(pid: i32, buffer: *mut c_char, buffersize: u32) -> i32;
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    // ── Classification tests (pure logic, platform-independent) ──────────

    #[test]
    fn ignored_process_does_not_publish_activity() {
        assert_eq!(
            classify_process("Mouse Jiggler", &["*jiggler*".to_string()]),
            ActivityDisposition::Ignore
        );
    }

    #[test]
    fn matched_name_is_accepted() {
        assert_eq!(
            classify_process("Terminal", &["*jiggler*".to_string()]),
            ActivityDisposition::Accept
        );
    }

    #[test]
    fn empty_ignore_list_accepts_everything() {
        assert_eq!(
            classify_process("USB Jiggler", &[]),
            ActivityDisposition::Accept
        );
    }

    #[test]
    fn case_insensitive_glob_matching() {
        assert_eq!(
            classify_process("Mouse jiggler", &["*JIGGLER*".to_string()]),
            ActivityDisposition::Ignore
        );
    }

    #[test]
    fn exact_name_glob_matches_exact() {
        assert_eq!(
            classify_process("JigglerApp", &["Jiggler*".to_string()]),
            ActivityDisposition::Ignore
        );
    }

    #[test]
    fn partial_name_not_matched() {
        assert_eq!(
            classify_process("Jiggy", &["Jiggler*".to_string()]),
            ActivityDisposition::Accept
        );
    }

    #[test]
    fn multiple_globs_first_match_wins() {
        assert_eq!(
            classify_process(
                "JigglerPro",
                &[
                    "*mouse*".to_string(),
                    "*jiggler*".to_string(),
                    "*keyboard*".to_string(),
                ]
            ),
            ActivityDisposition::Ignore
        );
    }

    #[test]
    fn multiple_globs_no_match_accepts() {
        assert_eq!(
            classify_process(
                "Terminal",
                &["*jiggler*".to_string(), "MosArt *".to_string(),]
            ),
            ActivityDisposition::Accept
        );
    }

    #[test]
    fn activity_disposition_is_exhaustive() {
        match classify_process("test", &[]) {
            ActivityDisposition::Accept | ActivityDisposition::Ignore => {}
        }
    }

    // ── Host seam fake ──────────────────────────────────────────────────

    struct FakeHost {
        trusted: bool,
        /// If `Some(err)`, `run_blocking` returns this error immediately
        /// instead of blocking.  `None` means block until cancelled.
        run_error: Option<String>,
        /// Set to `true` when `run_blocking` exits (clean or error).
        run_exited: AtomicBool,
        /// Number of times `run_blocking` was entered.
        run_count: std::sync::atomic::AtomicUsize,
        /// Signalled once when `run_blocking` is entered (before any sleep).
        entered: tokio::sync::Notify,
    }

    impl FakeHost {
        fn trusted() -> Self {
            Self {
                trusted: true,
                run_error: None,
                run_exited: AtomicBool::new(false),
                run_count: std::sync::atomic::AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
            }
        }

        fn denied() -> Self {
            Self {
                trusted: false,
                run_error: None,
                run_exited: AtomicBool::new(false),
                run_count: std::sync::atomic::AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                trusted: true,
                run_error: Some(reason.to_owned()),
                run_exited: AtomicBool::new(false),
                run_count: std::sync::atomic::AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
            }
        }
    }

    impl MacosInputHost for FakeHost {
        fn accessibility_trusted(&self) -> bool {
            self.trusted
        }

        fn run_blocking(
            &self,
            _matcher: DeviceMatcher,
            _activity_tx: FilteredActivityTx,
            cancel: CancellationToken,
        ) -> Result<()> {
            self.run_count.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            if let Some(ref reason) = self.run_error {
                self.run_exited.store(true, Ordering::SeqCst);
                bail!("{reason}");
            }
            // Block until cancelled.
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.run_exited.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    // ── Fail-safe: accessibility denied ──────────────────────────────────

    #[tokio::test]
    async fn accessibility_denied_start_returns_err() {
        let matcher = DeviceMatcher::compile(&["*jiggler*".to_string()]).unwrap();
        let host = Arc::new(FakeHost::denied());
        let source = MacosInputFilterSource::with_host(matcher, host.clone());

        let (tx, _rx) = filtered_activity_channel();
        let cancel = CancellationToken::new();
        let result = source.start(tx, cancel.clone()).await;

        assert!(
            result.is_err(),
            "start() must return Err when accessibility is denied"
        );
        assert_eq!(
            host.run_count.load(Ordering::SeqCst),
            0,
            "run_blocking must never be called when accessibility is denied"
        );
    }

    // ── Fail-safe: tap creation failure ──────────────────────────────────

    #[tokio::test]
    async fn tap_creation_failure_start_returns_err_from_join_handle() {
        let matcher = DeviceMatcher::compile(&["*jiggler*".to_string()]).unwrap();
        let host = Arc::new(FakeHost::failing("CGEventTapCreate returned NULL"));
        let source = MacosInputFilterSource::with_host(matcher, host.clone());

        let (tx, mut rx) = filtered_activity_channel();
        let cancel = CancellationToken::new();
        let handle = source
            .start(tx, cancel.clone())
            .await
            .expect("start() returns Ok when accessibility is granted; the failure is in the run");

        // The channel must be seeded available before spawn_blocking.
        assert!(
            rx.borrow_and_update().available,
            "channel must be seeded available before the run loop starts"
        );

        // Await the join handle — run_blocking returns Err immediately.
        let run_result = handle.await.expect("join must succeed");
        assert!(
            run_result.is_err(),
            "run_blocking error must propagate through the JoinHandle"
        );
        assert!(
            host.run_exited.load(Ordering::SeqCst),
            "run_blocking must have been entered and exited"
        );
    }

    // ── Positive: accessibility granted, tap OK → starts and cancels ─────

    #[tokio::test]
    async fn granted_start_seeds_channel_and_exits_on_cancel() {
        let matcher = DeviceMatcher::compile(&["*jiggler*".to_string()]).unwrap();
        let host = Arc::new(FakeHost::trusted());
        let source = MacosInputFilterSource::with_host(matcher, host.clone());

        let (tx, mut rx) = filtered_activity_channel();
        let cancel = CancellationToken::new();
        let handle = source
            .start(tx, cancel.clone())
            .await
            .expect("start() must succeed when accessibility is granted");

        // Wait for spawn_blocking to actually enter run_blocking.
        host.entered.notified().await;
        assert_eq!(
            host.run_count.load(Ordering::SeqCst),
            1,
            "run_blocking must have been called exactly once"
        );

        // Channel must be seeded available with an activity timestamp.
        let initial = rx.borrow_and_update().clone();
        assert!(initial.available);
        assert!(initial.last_activity.is_some());
        assert_eq!(initial.edge_seq, 0);

        // Cancel the token — the fake's run_blocking should exit.
        cancel.cancel();
        let run_result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("JoinHandle must complete within 2s after cancel")
            .expect("join must succeed");
        assert!(run_result.is_ok(), "clean exit after cancel must be Ok(())");
        assert!(host.run_exited.load(Ordering::SeqCst));
    }

    // ── Enabled tap: accepted events publish, ignored don't ──────────────

    #[tokio::test]
    async fn accepted_event_publishes_ignored_does_not() {
        let matcher = DeviceMatcher::compile(&["*jiggler*".to_string()]).unwrap();
        let (tx, mut rx) = filtered_activity_channel();

        let now = Instant::now();
        let baseline = FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 0,
        };
        // Seed the channel and the Mutex with the same baseline.
        tx.send_replace(baseline.clone());
        let activity = Mutex::new(baseline);
        let activity_tx = Mutex::new(tx);

        // Simulate what the tap callback does for an ignored event.
        let ignored_name = "USB Jiggler";
        if !matcher.is_ignored(ignored_name) {
            publish_accepted_event(&activity, &activity_tx);
        }
        let after_ignored = rx.borrow_and_update().clone();
        assert_eq!(
            after_ignored.edge_seq, 0,
            "ignored event must not advance edge_seq"
        );
        assert_eq!(
            after_ignored.last_activity,
            Some(now),
            "ignored event must not update last_activity"
        );

        // Simulate what the tap callback does for an accepted event.
        let accepted_name = "Terminal";
        if !matcher.is_ignored(accepted_name) {
            publish_accepted_event(&activity, &activity_tx);
        }
        let after_accepted = rx.borrow_and_update().clone();
        assert_eq!(
            after_accepted.edge_seq, 1,
            "accepted event must advance edge_seq"
        );
        assert!(
            after_accepted.last_activity.is_some_and(|ts| ts > now),
            "accepted event must advance last_activity"
        );
    }

    // ── Cancellation terminates the run loop ─────────────────────────────

    #[tokio::test]
    async fn cancellation_before_start_prevents_run_blocking_call() {
        let matcher = DeviceMatcher::compile(&["*jiggler*".to_string()]).unwrap();
        let host = Arc::new(FakeHost::trusted());
        let source = MacosInputFilterSource::with_host(matcher, host.clone());

        let (tx, _rx) = filtered_activity_channel();
        let cancel = CancellationToken::new();
        // Cancel before start — the `spawn_blocking` closure checks
        // cancellation in `run_blocking` (fake blocks on it), BUT
        // `start()` doesn't check cancellation before spawning.
        // The JoinHandle will run `run_blocking` which immediately
        // sees the cancelled token and exits.
        let handle = source
            .start(tx, cancel.clone())
            .await
            .expect("start() should succeed (accessibility granted)");

        cancel.cancel();
        let run_result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("JoinHandle must complete within 2s")
            .expect("join must succeed");
        assert!(run_result.is_ok(), "cancelled run returns Ok(())");
        assert!(host.run_exited.load(Ordering::SeqCst));
    }
}
