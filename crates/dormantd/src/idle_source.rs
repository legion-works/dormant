//! Idle sources for the activity inhibitor.
//!
//! Defines the [`IdleSource`] trait that the inhibitor uses to detect user
//! activity/inactivity, plus two implementations:
//!
//! * [`DbusIdleSource`] — polls `org.freedesktop.ScreenSaver.GetSessionIdleTime`
//!   over the session bus. Used as the X11 / non-Wayland fallback. Includes the
//!   ms/s unit-detection heuristic (the Wayland path has no unit ambiguity).
//! * [`WaylandIdleNotifier`] — connects to the compositor's
//!   `ext_idle_notifier_v1` global and listens for `idled` / `resumed` events.
//!
//! Both implementations follow the fail-toward-normal-blanking rule: any error
//! or unavailability treats the user as **inactive** (inhibitor OFF → blanking
//! ALLOWED). A broken idle probe must never wedge displays awake.

// Linux owns the D-Bus and Wayland idle implementations. Non-Linux builds
// retain their shared test seams and platform fallbacks without reaching them.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;
use std::time::Duration;

use dormant_core::config::IdleTimeUnit;
use dormant_core::rules::{ControlMsg, InhibitorKind};
use dormant_core::types::RuleId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ── ActivityRule ───────────────────────────────────────────────────────────────

/// One rule that declares the `user-activity` inhibitor, with its idle threshold.
#[derive(Debug, Clone)]
pub struct ActivityRule {
    /// The rule this inhibitor gates.
    pub rule: RuleId,
    /// Idle time below which the user is considered active (inhibited).
    pub idle_threshold: Duration,
}

// ── IdleSource trait ───────────────────────────────────────────────────────────

/// An idle source detecting user activity/inactivity.
///
/// Implementations must be fail-safe: any error or unavailability inside `run`
/// must treat the user as inactive (set `inhibited = false`) rather than holding
/// displays awake — a broken idle probe must never wedge displays awake.
#[async_trait::async_trait]
pub trait IdleSource: Send + 'static {
    /// Run the idle source, publishing per-rule inhibition state via `ctl`.
    ///
    /// Returns when `cancel` fires. On failure the source must internally handle
    /// retries and revert to inactive while doing so; the return value is only
    /// for logging.
    async fn run(self: Box<Self>, ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken);
}

// ── DBus idle source ───────────────────────────────────────────────────────────

/// Reconnect / retry interval after a session-bus failure.
const DBUS_RECONNECT_INTERVAL: Duration = Duration::from_secs(60);

/// The resolved interpretation of a raw idle value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedUnit {
    /// Raw value is milliseconds.
    Ms,
    /// Raw value is seconds.
    Secs,
}

impl ResolvedUnit {
    /// Convert a raw idle reading into milliseconds.
    fn to_ms(self, raw: u64) -> u64 {
        match self {
            Self::Ms => raw,
            Self::Secs => raw.saturating_mul(1000),
        }
    }
}

/// Map the configured unit to a resolved unit, or `None` for `auto`.
fn configured_unit(unit: IdleTimeUnit) -> Option<ResolvedUnit> {
    match unit {
        IdleTimeUnit::Auto => None,
        IdleTimeUnit::Ms => Some(ResolvedUnit::Ms),
        IdleTimeUnit::Secs => Some(ResolvedUnit::Secs),
    }
}

/// Number of corroborating idle-growth samples required before locking the
/// unit under `auto`. Guards against a single active-user jitter delta
/// (e.g. 10→13) that happens to land near the poll interval in one unit.
const CORROBORATION: u32 = 2;

/// Classify a single inter-poll delta: it must fall within ±25% of the poll
/// interval in **exactly one** unit interpretation. Deltas matching both or
/// neither (tiny jitter, huge jumps) are inconclusive.
fn classify_delta(delta: u64, poll: Duration) -> Option<ResolvedUnit> {
    let ms = u64::try_from(poll.as_millis()).unwrap_or(u64::MAX);
    let secs = poll.as_secs();
    let ms_match = within_25(delta, ms);
    let secs_match = secs > 0 && within_25(delta, secs);
    match (ms_match, secs_match) {
        (true, false) => Some(ResolvedUnit::Ms),
        (false, true) => Some(ResolvedUnit::Secs),
        _ => None,
    }
}

/// Whether `a` is within `[0.75·b, 1.25·b]` (i.e. `4a ∈ [3b, 5b]`).
fn within_25(a: u64, b: u64) -> bool {
    if b == 0 {
        return false;
    }
    let four_a = a.saturating_mul(4);
    four_a >= b.saturating_mul(3) && four_a <= b.saturating_mul(5)
}

/// Detects (or is told) the idle-value unit. Under `auto` it requires
/// [`CORROBORATION`] consecutive samples agreeing on the same unit before
/// locking; a conflicting or inconclusive sample resets the streak. Until a
/// unit is locked the caller keeps the inhibitor inactive.
#[derive(Debug)]
struct UnitDetector {
    poll: Duration,
    prev_raw: Option<u64>,
    streak: Option<(ResolvedUnit, u32)>,
    locked: Option<ResolvedUnit>,
}

impl UnitDetector {
    fn new(poll: Duration, forced: Option<ResolvedUnit>) -> Self {
        Self {
            poll,
            prev_raw: None,
            streak: None,
            locked: forced,
        }
    }

    /// Feed a raw sample; returns the locked unit once determined.
    fn observe(&mut self, raw: u64) -> Option<ResolvedUnit> {
        if let Some(u) = self.locked {
            return Some(u);
        }
        let candidate = self
            .prev_raw
            .map(|prev| raw.saturating_sub(prev))
            .and_then(|delta| classify_delta(delta, self.poll));
        self.prev_raw = Some(raw);

        match candidate {
            Some(unit) => {
                let count = match self.streak {
                    Some((u, n)) if u == unit => n + 1,
                    _ => 1,
                };
                self.streak = Some((unit, count));
                if count >= CORROBORATION {
                    self.locked = Some(unit);
                }
            }
            None => self.streak = None,
        }
        self.locked
    }
}

/// Polls `org.freedesktop.ScreenSaver.GetSessionIdleTime` on the session bus
/// and publishes per-rule inhibition state. Handles unit detection (ms vs s)
/// under the existing `daemon.idle_time_unit` config key.
pub struct DbusIdleSource {
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    unit: IdleTimeUnit,
}

impl DbusIdleSource {
    /// Create a `DBus` idle source.
    #[must_use]
    pub fn new(rules: Vec<ActivityRule>, poll_interval: Duration, unit: IdleTimeUnit) -> Self {
        Self {
            rules,
            poll_interval,
            unit,
        }
    }
}

#[async_trait::async_trait]
impl IdleSource for DbusIdleSource {
    async fn run(self: Box<Self>, ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
        dbus_run(self.rules, self.poll_interval, self.unit, ctl, cancel).await;
    }
}

/// Run the `DBus` idle poller.
#[cfg(target_os = "linux")]
async fn dbus_run(
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    unit: IdleTimeUnit,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    let mut conn: Option<zbus::Connection> = None;
    let mut last_sent: HashMap<RuleId, bool> = HashMap::new();
    let mut detector = UnitDetector::new(poll_interval, configured_unit(unit));
    let mut warned_offline = false;
    let mut warned_undetermined = false;

    loop {
        // Establish (or re-establish) the session-bus connection.
        if conn.is_none() {
            match zbus::Connection::session().await {
                Ok(c) => {
                    conn = Some(c);
                    warned_offline = false;
                }
                Err(e) => {
                    if !warned_offline {
                        tracing::warn!(
                            event = "activity_inhibitor_unreachable",
                            error = %e,
                            "session bus unreachable; treating user as inactive, retry in 60s",
                        );
                        warned_offline = true;
                    }
                    set_all_inactive(&ctl, &mut last_sent, &rules);
                    if sleep_or_cancel(DBUS_RECONNECT_INTERVAL, &cancel).await {
                        return;
                    }
                    continue;
                }
            }
        }

        match get_idle_raw(conn.as_ref().expect("connection present")).await {
            Ok(raw) => {
                tracing::info!(
                    event = "idle_probe_raw",
                    raw = raw,
                    interp_ms = raw,
                    interp_s = raw,
                );

                let was_locked = detector.locked.is_some();
                if let Some(unit) = detector.observe(raw) {
                    if !was_locked {
                        tracing::info!(event = "idle_unit_determined", unit = ?unit);
                    }
                    let idle = Duration::from_millis(unit.to_ms(raw));
                    for r in &rules {
                        let inhibited = idle < r.idle_threshold;
                        publish(&ctl, &mut last_sent, &r.rule, inhibited);
                    }
                } else {
                    if !warned_undetermined {
                        tracing::warn!(
                            event = "idle_unit_undetermined",
                            "idle-time unit not yet determined; treating user as inactive",
                        );
                        warned_undetermined = true;
                    }
                    set_all_inactive(&ctl, &mut last_sent, &rules);
                }
            }
            Err(e) => {
                if !warned_offline {
                    tracing::warn!(
                        event = "activity_inhibitor_probe_failed",
                        error = %e,
                        "idle probe failed or unsupported; treating user as inactive, retry in 60s",
                    );
                    warned_offline = true;
                }
                conn = None;
                set_all_inactive(&ctl, &mut last_sent, &rules);
                if sleep_or_cancel(DBUS_RECONNECT_INTERVAL, &cancel).await {
                    return;
                }
                continue;
            }
        }

        if sleep_or_cancel(poll_interval, &cancel).await {
            return;
        }
    }
}

/// Non-Linux stub — no session bus.
#[cfg(not(target_os = "linux"))]
async fn dbus_run(
    _rules: Vec<ActivityRule>,
    _poll_interval: Duration,
    _unit: IdleTimeUnit,
    _ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    cancel.cancelled().await;
}

/// Publish `inhibited = false` for every rule (fail-toward-blanking).
///
/// Platform-neutral: shared by the Linux `DBus`/Wayland sources and the
/// macOS `CoreGraphics` source (`crate::macos_idle::macos_run`) — not
/// `cfg`-gated so it stays testable wherever those callers are.
pub(crate) fn set_all_inactive(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<RuleId, bool>,
    rules: &[ActivityRule],
) {
    for r in rules {
        publish(ctl, last_sent, &r.rule, false);
    }
}

/// Send a `SetInhibited` only when the value changed for this rule, and record
/// the new value **only on a successful send** so a dropped edge is retried on
/// the next poll rather than silently lost.
///
/// Platform-neutral for the same reason as [`set_all_inactive`].
pub(crate) fn publish(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<RuleId, bool>,
    rule: &RuleId,
    inhibited: bool,
) {
    if last_sent.get(rule) == Some(&inhibited) {
        return;
    }
    if ctl
        .try_send(ControlMsg::SetInhibited {
            rule: Some(rule.clone()),
            kind: InhibitorKind::UserActivity,
            inhibited,
        })
        .is_ok()
    {
        last_sent.insert(rule.clone(), inhibited);
    }
}

/// Query `GetSessionIdleTime` (raw units) from the screensaver service.
#[cfg(target_os = "linux")]
async fn get_idle_raw(conn: &zbus::Connection) -> zbus::Result<u64> {
    let reply = conn
        .call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/org/freedesktop/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "GetSessionIdleTime",
            &(),
        )
        .await?;
    let idle: u32 = reply.body().deserialize()?;
    Ok(u64::from(idle))
}

/// Sleep for `dur` or return `true` if cancellation fired first.
///
/// Platform-neutral for the same reason as [`set_all_inactive`] — also used
/// by `crate::macos_idle::macos_run` to make cancellation interrupt the
/// poll sleep there too.
pub(crate) async fn sleep_or_cancel(dur: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(dur) => false,
    }
}

// ── Wayland idle source ────────────────────────────────────────────────────────

/// Connects to the compositor's `ext_idle_notifier_v1` global and listens for
/// `idled` / `resumed` events. The Wayland event loop runs in a blocking thread;
/// idled → inactive (not inhibited), resumed → active (inhibited).
#[cfg(target_os = "linux")]
pub struct WaylandIdleNotifier {
    rules: Vec<ActivityRule>,
    timeout_ms: u32,
}

#[cfg(target_os = "linux")]
impl WaylandIdleNotifier {
    /// Create a Wayland idle notifier.
    ///
    /// `timeout` is the idle threshold to register with the compositor.
    #[must_use]
    pub fn new(rules: Vec<ActivityRule>, timeout: Duration) -> Self {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        Self { rules, timeout_ms }
    }

    /// Check whether the compositor advertises `ext_idle_notifier_v1`.
    ///
    /// `true` when `WAYLAND_DISPLAY` is set and the global is present.
    /// A transient failure (display unreachable) returns `false` — the caller
    /// should fall back to `DBus`.
    #[must_use]
    pub fn available() -> bool {
        check_wayland_available().unwrap_or(false)
    }
}

/// Probe the compositor for `ext_idle_notifier_v1` availability.
#[cfg(target_os = "linux")]
#[allow(clippy::items_after_statements)] // AvailState + Dispatch impls are local to this probe
fn check_wayland_available() -> Result<bool, Box<dyn std::error::Error>> {
    use wayland_client::{
        Connection, Dispatch, QueueHandle,
        protocol::{wl_registry, wl_seat},
    };

    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue();
    let display = conn.display();
    let _registry = display.get_registry(&event_queue.handle(), ());

    #[derive(Default)]
    struct AvailState {
        idle_notifier: bool,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for AvailState {
        fn event(
            state: &mut Self,
            _: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { interface, .. } = event
                && interface == "ext_idle_notifier_v1"
            {
                state.idle_notifier = true;
            }
        }
    }

    // wl_seat dispatch is needed only so the registry roundtrip resolves
    // all globals; we don't actually use the seat in the probe.
    impl Dispatch<wl_seat::WlSeat, ()> for AvailState {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            _: wl_seat::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    // Roundtrip to ensure all initial globals (including ext_idle_notifier_v1)
    // have been received before we check availability.
    let mut probe_state = AvailState::default();
    event_queue.roundtrip(&mut probe_state)?;

    Ok(probe_state.idle_notifier)
}

// ── WlState + Dispatch impls (Linux-only) ──────────────────────────────────────

/// Wayland-side state machine for the idle notification event loop.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct WlState {
    idled_sender: Option<tokio::sync::mpsc::UnboundedSender<bool>>,
}

#[cfg(target_os = "linux")]
mod wl_dispatch {
    //! Dispatch implementations for `WlState` — one per protocol object.

    use super::WlState;
    use wayland_client::{
        Connection, Dispatch, QueueHandle,
        globals::GlobalListContents,
        protocol::{wl_registry, wl_seat},
    };
    use wayland_protocols::ext::idle_notify::v1::client::{
        ext_idle_notification_v1::ExtIdleNotificationV1, ext_idle_notifier_v1::ExtIdleNotifierV1,
    };

    // Registry dispatch: `registry_queue_init` handles globals internally;
    // we just need the trait bound satisfied.
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WlState {
        fn event(
            _state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for WlState {
        fn event(
            _state: &mut Self,
            _proxy: &wl_seat::WlSeat,
            _event: wl_seat::Event,
            _data: &(),
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotifierV1, ()> for WlState {
        fn event(
            _state: &mut Self,
            _proxy: &ExtIdleNotifierV1,
            _event: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotificationV1, ()> for WlState {
        fn event(
            state: &mut Self,
            _proxy: &ExtIdleNotificationV1,
            event: <ExtIdleNotificationV1 as wayland_client::Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
            use ExtIdleNotificationV1 as IdleN;
            match event {
                <IdleN as wayland_client::Proxy>::Event::Idled => {
                    if let Some(ref tx) = state.idled_sender {
                        let _ = tx.send(true);
                    }
                }
                <IdleN as wayland_client::Proxy>::Event::Resumed => {
                    if let Some(ref tx) = state.idled_sender {
                        let _ = tx.send(false);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Implementation stubs — the real implementation lives in the blocking task.
#[cfg(not(target_os = "linux"))]
pub struct WaylandIdleNotifier;

#[cfg(not(target_os = "linux"))]
impl WaylandIdleNotifier {
    #[must_use]
    pub fn new(_rules: Vec<ActivityRule>, _timeout: Duration) -> Self {
        Self
    }

    #[must_use]
    pub fn available() -> bool {
        false
    }
}

#[async_trait::async_trait]
#[cfg(target_os = "linux")]
impl IdleSource for WaylandIdleNotifier {
    async fn run(self: Box<Self>, ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
        wayland_run(self.rules, self.timeout_ms, ctl, cancel).await;
    }
}

#[async_trait::async_trait]
#[cfg(not(target_os = "linux"))]
impl IdleSource for WaylandIdleNotifier {
    async fn run(self: Box<Self>, _ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
        cancel.cancelled().await;
    }
}

// ── wayland_run ────────────────────────────────────────────────────────────────

/// Run the Wayland idle notification event loop in a blocking thread.
///
/// On `idled` → publish all rules as inactive (not inhibited).
/// On `resumed` → publish all rules as active (inhibited).
/// On error or compositor disconnect → treat as inactive and retry.
///
/// The blocking thread is verified live on hardware (Wayland compositor); the
/// async fail-safe wiring (cancel, channel-close→inactive) is testable in CI.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_lines)] // the blocking closure + async orchestration is co-located intentionally
async fn wayland_run(
    rules: Vec<ActivityRule>,
    timeout_ms: u32,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let timeout = timeout_ms;
    let mut warned_offline = false;

    // Outer reconnect loop: if the Wayland connection drops, wait and reconnect
    // rather than letting the inhibitor task exit (MUST 3 fix).
    loop {
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();

        // Cancel flag shared with the blocking thread so it can interrupt polling.
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let cancel_flag_clone = Arc::clone(&cancel_flag);
        let wl_handle = tokio::task::spawn_blocking::<_, Result<(), String>>(move || {
            use rustix::event::{PollFd, PollFlags, poll};
            use rustix::time::Timespec;
            use wayland_client::{Connection, globals::registry_queue_init, protocol::wl_seat};
            use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

            let conn = Connection::connect_to_env().map_err(|e| format!("wayland connect: {e}"))?;

            let (globals, mut event_queue) = registry_queue_init::<WlState>(&conn)
                .map_err(|e| format!("wayland registry: {e}"))?;
            let qh = event_queue.handle();

            // Bind the wake seat — used only as a parameter to get_idle_notification.
            let seat: wl_seat::WlSeat = globals
                .bind(&qh, 1..=7, ())
                .map_err(|e| format!("bind wl_seat: {e}"))?;

            // Bind the idle notifier.
            let notifier: ExtIdleNotifierV1 = globals
                .bind(&qh, 1..=1, ())
                .map_err(|e| format!("bind ext_idle_notifier_v1: {e}"))?;

            // Create the idle notification with our timeout.
            notifier.get_idle_notification(timeout, &seat, &qh, ());

            // Roundtrip to ensure the notification request was sent.
            let mut state = WlState {
                idled_sender: Some(ev_tx),
            };
            event_queue
                .roundtrip(&mut state)
                .map_err(|e| format!("wayland roundtrip: {e}"))?;

            // Polling-based event loop — interruptible via cancel_flag.
            loop {
                // Dispatch any events buffered from previous reads or other threads.
                event_queue
                    .dispatch_pending(&mut state)
                    .map_err(|e| format!("dispatch_pending: {e}"))?;

                // Flush outgoing requests.
                event_queue.flush().map_err(|e| format!("flush: {e}"))?;

                // Prepare to read from the Wayland socket.
                let read_guard = event_queue.prepare_read().ok_or_else(|| {
                    "prepare_read returned None — queue already reading".to_string()
                })?;

                let fd = read_guard.connection_fd();
                let mut pollfds = [PollFd::new(&fd, PollFlags::IN)];

                let poll_timeout = Timespec {
                    tv_sec: 0,
                    tv_nsec: 200_000_000, // 200 ms
                };
                match poll(&mut pollfds, Some(&poll_timeout)) {
                    // Timeout or interrupted — check cancel flag, loop.
                    Ok(0) | Err(rustix::io::Errno::INTR) => {
                        drop(read_guard);
                        if cancel_flag_clone.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        continue;
                    }
                    Err(e) => {
                        drop(read_guard);
                        return Err(format!("poll error: {e}"));
                    }
                    Ok(_) => {
                        // Socket readable — ingest events.
                        read_guard.read().map_err(|e| format!("read_events: {e}"))?;
                    }
                }

                // Dispatch the newly-read events.
                event_queue
                    .dispatch_pending(&mut state)
                    .map_err(|e| format!("dispatch_pending(2): {e}"))?;

                if cancel_flag_clone.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
        });

        // Inner loop: wait for Wayland events or cancellation within one connection.
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    cancel_flag.store(true, Ordering::Relaxed);
                    wl_handle.abort();
                    return;
                }
                event = ev_rx.recv() => {
                    if let Some(idled) = event {
                        // idled = true → user is idle (inactive, not inhibited)
                        // idled = false → user resumed (active, inhibited)
                        for r in &rules {
                            let inhibited = !idled;
                            let _ = ctl.try_send(ControlMsg::SetInhibited {
                                rule: Some(r.rule.clone()),
                                kind: InhibitorKind::UserActivity,
                                inhibited,
                            });
                        }
                        if warned_offline {
                            tracing::info!(event = "wayland_idle_reconnected");
                            warned_offline = false;
                        }
                    } else {
                        // Channel closed — Wayland thread exited.
                        if !warned_offline {
                            tracing::warn!(
                                event = "wayland_idle_disconnected",
                                "Wayland idle source disconnected; treating user as inactive",
                            );
                            warned_offline = true;
                        }
                        // Treat as inactive (fail-toward-blanking).
                        for r in &rules {
                            let _ = ctl.try_send(ControlMsg::SetInhibited {
                                rule: Some(r.rule.clone()),
                                kind: InhibitorKind::UserActivity,
                                inhibited: false,
                            });
                        }
                        // Reconnect after a delay, then respawn via the outer loop.
                        tokio::select! {
                            () = cancel.cancelled() => return,
                            () = tokio::time::sleep(DBUS_RECONNECT_INTERVAL) => {},
                        }
                        break;
                    }
                }
            }
        }
    }
}

// ── Source detection ───────────────────────────────────────────────────────────

/// Select and create the appropriate idle source based on the configured mode
/// and environment.
///
/// `macos_guard_cfg` carries the `daemon.macos_idle_*` knobs through to the
/// macOS source's [`crate::macos_idle::MacosIdleGuard`]; it's plain data on
/// every platform (not `cfg`-gated) so callers don't need their own `cfg` to
/// build one, even though it's only consulted when `effective` resolves to
/// `Macos` on an actual macOS build.
///
/// Returns `None` when there are no rules.
#[must_use]
pub fn create_source(
    mode: dormant_core::config::IdleSource,
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    idle_unit: IdleTimeUnit,
    macos_guard_cfg: crate::macos_idle::MacosIdleGuardConfig,
) -> Option<Box<dyn IdleSource>> {
    if rules.is_empty() {
        return None;
    }
    // `MacosIdleGuardConfig` is `Copy`; this "use" only silences the
    // unused-variable warning on non-macOS builds, where the match arm
    // below that actually consumes it is `cfg`-gated out — it doesn't
    // consume the value the `Macos` arm still needs on macOS.
    let _ = macos_guard_cfg;

    let effective = match mode {
        #[cfg(target_os = "linux")]
        dormant_core::config::IdleSource::Wayland => {
            if WaylandIdleNotifier::available() {
                dormant_core::config::IdleSource::Wayland
            } else {
                tracing::warn!(
                    event = "wayland_idle_unavailable",
                    "wayland idle source requested but ext_idle_notifier_v1 not found; \
                     treating user as inactive",
                );
                // Fall back to DBus rather than fail entirely.
                dormant_core::config::IdleSource::Dbus
            }
        }
        #[cfg(not(target_os = "linux"))]
        dormant_core::config::IdleSource::Wayland => {
            tracing::warn!(
                event = "wayland_idle_unsupported",
                "wayland idle source not available on this platform",
            );
            dormant_core::config::IdleSource::Dbus
        }
        #[cfg(target_os = "linux")]
        dormant_core::config::IdleSource::Auto => {
            if WaylandIdleNotifier::available() {
                tracing::info!(event = "idle_source_selected", source = "wayland");
                dormant_core::config::IdleSource::Wayland
            } else {
                tracing::info!(event = "idle_source_selected", source = "dbus");
                dormant_core::config::IdleSource::Dbus
            }
        }
        // Auto selects Macos ON MACOS ONLY — Linux Auto (above) stays
        // Wayland→DBus byte-identically, and this arm never fires there.
        #[cfg(target_os = "macos")]
        dormant_core::config::IdleSource::Auto => {
            tracing::info!(event = "idle_source_selected", source = "macos");
            dormant_core::config::IdleSource::Macos
        }
        // Windows (or any other non-Linux, non-macOS target): keep the
        // existing unsupported/fail-toward-inactive behavior — fall back to
        // the DBus source, whose non-Linux stub (`dbus_run` below) never
        // publishes anything, so rules simply stay at their inactive
        // default rather than wedging displays awake.
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        dormant_core::config::IdleSource::Auto => {
            tracing::info!(event = "idle_source_selected", source = "dbus");
            dormant_core::config::IdleSource::Dbus
        }
        dormant_core::config::IdleSource::Dbus => dormant_core::config::IdleSource::Dbus,
        // Explicitly configured on macOS: use the real CoreGraphics source.
        #[cfg(target_os = "macos")]
        dormant_core::config::IdleSource::Macos => {
            tracing::info!(event = "idle_source_selected", source = "macos");
            dormant_core::config::IdleSource::Macos
        }
        // Explicitly configured on a foreign (non-macOS) target: this is
        // the same "explicit unsupported" pattern the Wayland arm above
        // uses for non-Linux targets (`wayland_idle_unsupported` → warn +
        // fall back to DBus rather than fail to start) — mirrored here so
        // an operator who forces `idle_source = "macos"` on Linux/Windows
        // gets a clear warning instead of a silent no-op or a startup
        // failure. Replaces Task 5's interim `macos_idle_not_yet_wired`
        // warn+DBus-fallback arm now that the real backend exists.
        #[cfg(not(target_os = "macos"))]
        dormant_core::config::IdleSource::Macos => {
            tracing::warn!(
                event = "macos_idle_unsupported",
                "macos idle source requested but this platform is not macOS; \
                 treating user as inactive via dbus fallback",
            );
            dormant_core::config::IdleSource::Dbus
        }
    };

    match effective {
        #[cfg(target_os = "linux")]
        dormant_core::config::IdleSource::Wayland => {
            // Use the minimum threshold across all rules as the Wayland timeout.
            let min_threshold = rules
                .iter()
                .map(|r| r.idle_threshold)
                .min()
                .unwrap_or(Duration::from_secs(120));
            Some(Box::new(WaylandIdleNotifier::new(rules, min_threshold)))
        }
        #[cfg(not(target_os = "linux"))]
        dormant_core::config::IdleSource::Wayland => Some(Box::new(DbusIdleSource::new(
            rules,
            poll_interval,
            idle_unit,
        ))),
        dormant_core::config::IdleSource::Dbus => Some(Box::new(DbusIdleSource::new(
            rules,
            poll_interval,
            idle_unit,
        ))),
        #[cfg(target_os = "macos")]
        dormant_core::config::IdleSource::Macos => Some(Box::new(
            crate::macos_idle::MacosIdleSource::new(rules, poll_interval, macos_guard_cfg),
        )),
        // Auto/Macos resolved to one of the above on this platform, or
        // Macos degraded to Dbus above — unreachable in practice.
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit detector tests (copied from inhibit_activity.rs) ────────────────

    const POLL: Duration = Duration::from_secs(5);

    fn detect(samples: &[u64]) -> Option<ResolvedUnit> {
        let mut d = UnitDetector::new(POLL, None);
        let mut last = None;
        for &s in samples {
            last = d.observe(s);
        }
        last
    }

    #[test]
    fn active_user_jitter_stays_undetermined() {
        // 10→13 @5s: delta 3 is near neither 5000 (ms) nor 5 (s).
        assert_eq!(classify_delta(3, POLL), None);
        assert_eq!(detect(&[10, 13]), None);
    }

    #[test]
    fn locks_milliseconds_after_two_corroborating_samples() {
        // Two idle-growth deltas of ~5000.
        assert_eq!(detect(&[0, 5_000, 10_000]), Some(ResolvedUnit::Ms));
    }

    #[test]
    fn locks_seconds_after_two_corroborating_samples() {
        // Two idle-growth deltas of ~5.
        assert_eq!(detect(&[0, 5, 10]), Some(ResolvedUnit::Secs));
    }

    #[test]
    fn conflicting_streak_resets_and_does_not_lock() {
        // delta 5000 (ms) then delta 5 (s) — conflicting single samples.
        assert_eq!(detect(&[0, 5_000, 5_005]), None);
    }

    #[test]
    fn single_sample_never_locks() {
        assert_eq!(detect(&[0, 5_000]), None);
    }

    #[test]
    fn forced_unit_locks_immediately() {
        let mut d = UnitDetector::new(POLL, Some(ResolvedUnit::Ms));
        assert_eq!(d.observe(42), Some(ResolvedUnit::Ms));
    }

    #[test]
    fn classify_delta_picks_exactly_one_unit() {
        assert_eq!(classify_delta(5_000, POLL), Some(ResolvedUnit::Ms));
        assert_eq!(classify_delta(5, POLL), Some(ResolvedUnit::Secs));
        assert_eq!(classify_delta(100, POLL), None);
    }

    #[test]
    fn to_ms_scales_seconds() {
        assert_eq!(ResolvedUnit::Ms.to_ms(1500), 1500);
        assert_eq!(ResolvedUnit::Secs.to_ms(3), 3000);
    }

    #[test]
    fn within_25_bounds() {
        assert!(within_25(5000, 5000));
        assert!(within_25(3750, 5000));
        assert!(within_25(6250, 5000));
        assert!(!within_25(3000, 5000));
        assert!(!within_25(7000, 5000));
        assert!(!within_25(5, 0));
    }

    // ── IdleSource trait tests with fakes ───────────────────────────────────

    /// A fake idle source for unit testing source selection and inhibitor logic.
    struct FakeIdleSource {
        /// Values to emit on each poll; drained in order.
        events: Vec<bool>,
    }

    #[async_trait::async_trait]
    impl IdleSource for FakeIdleSource {
        async fn run(self: Box<Self>, ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
            for inhibited in self.events {
                if cancel.is_cancelled() {
                    return;
                }
                // Pretend each event publishes to all rules (just one in tests).
                let _ = ctl.try_send(ControlMsg::SetInhibited {
                    rule: Some(RuleId("test".into())),
                    kind: InhibitorKind::UserActivity,
                    inhibited,
                });
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancelled().await;
        }
    }

    #[tokio::test]
    async fn fake_source_publishes_inhibited_events() {
        let (ctl, mut ctl_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let source: Box<dyn IdleSource> = Box::new(FakeIdleSource {
            events: vec![true, false, true],
        });

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { source.run(ctl, cancel_clone).await });

        // Should receive three events.
        let mut events = Vec::new();
        for _ in 0..3 {
            if let Some(ControlMsg::SetInhibited { inhibited, .. }) = ctl_rx.recv().await {
                events.push(inhibited);
            }
        }
        cancel.cancel();
        handle.await.ok();

        assert_eq!(events, vec![true, false, true]);
    }

    // ── Source selection tests ───────────────────────────────────────────────

    #[test]
    fn create_source_no_rules_returns_none() {
        let result = create_source(
            dormant_core::config::IdleSource::Auto,
            vec![],
            Duration::from_secs(5),
            IdleTimeUnit::Auto,
            crate::macos_idle::MacosIdleGuardConfig::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn create_source_dbus_returns_some() {
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(120),
        }];
        let result = create_source(
            dormant_core::config::IdleSource::Dbus,
            rules,
            Duration::from_secs(5),
            IdleTimeUnit::Auto,
            crate::macos_idle::MacosIdleGuardConfig::default(),
        );
        assert!(result.is_some());
    }

    #[test]
    fn create_source_auto_falls_back_to_dbus() {
        // On non-Linux CI, Auto should fall back to DBus.
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(120),
        }];
        let result = create_source(
            dormant_core::config::IdleSource::Auto,
            rules,
            Duration::from_secs(5),
            IdleTimeUnit::Auto,
            crate::macos_idle::MacosIdleGuardConfig::default(),
        );
        // Auto resolves — always returns Some when rules are non-empty.
        assert!(result.is_some());
    }

    #[test]
    fn create_source_wayland_falls_back_when_not_available() {
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(120),
        }];
        let result = create_source(
            dormant_core::config::IdleSource::Wayland,
            rules,
            Duration::from_secs(5),
            IdleTimeUnit::Auto,
            crate::macos_idle::MacosIdleGuardConfig::default(),
        );
        // Wayland mode either succeeds or falls back to DBus — always Some.
        assert!(result.is_some());
    }

    // ── macOS selection tests ────────────────────────────────────────────────

    /// Explicitly-configured `idle_source = "macos"` on a foreign (non-macOS)
    /// target must not silently no-op or fail to start — it degrades to
    /// `DBus` the same way `Wayland` does on non-Linux (see
    /// `create_source`'s `macos_idle_unsupported` arm). Linux-runnable;
    /// the macOS-target counterpart (selects the real `MacosIdleSource`) is
    /// exercised on the macOS CI lane.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn create_source_macos_explicit_on_foreign_target_falls_back_to_dbus() {
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(120),
        }];
        let result = create_source(
            dormant_core::config::IdleSource::Macos,
            rules,
            Duration::from_secs(5),
            IdleTimeUnit::Auto,
            crate::macos_idle::MacosIdleGuardConfig::default(),
        );
        // Falls back to DBus rather than returning None or panicking.
        assert!(result.is_some());
    }

    /// `Auto` on a foreign target must never resolve to `Macos` — it stays
    /// on the existing Linux Wayland→DBus (or non-Linux `DBus`) path.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn create_source_auto_on_foreign_target_never_selects_macos() {
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(120),
        }];
        let result = create_source(
            dormant_core::config::IdleSource::Auto,
            rules,
            Duration::from_secs(5),
            IdleTimeUnit::Auto,
            crate::macos_idle::MacosIdleGuardConfig::default(),
        );
        assert!(result.is_some());
    }

    // ── Fail-safe test ───────────────────────────────────────────────────────

    /// A fake idle source that simulates a broken probe — always errors.
    struct FailingIdleSource;

    #[async_trait::async_trait]
    impl IdleSource for FailingIdleSource {
        async fn run(self: Box<Self>, ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
            // Simulate a broken probe: immediately set all rules inactive, then exit.
            let _ = ctl.try_send(ControlMsg::SetInhibited {
                rule: Some(RuleId("test".into())),
                kind: InhibitorKind::UserActivity,
                inhibited: false,
            });
            cancel.cancelled().await;
        }
    }

    #[tokio::test]
    async fn failing_source_reports_inactive() {
        let (ctl, mut ctl_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let source: Box<dyn IdleSource> = Box::new(FailingIdleSource);

        let cancel_clone = cancel.clone();
        tokio::spawn(async move { source.run(ctl, cancel_clone).await });

        let msg = ctl_rx.recv().await.unwrap();
        if let ControlMsg::SetInhibited { inhibited, .. } = msg {
            // Fail-safe: broken probe → inactive (not inhibited).
            assert!(!inhibited);
        } else {
            panic!("expected SetInhibited");
        }

        cancel.cancel();
    }
}
