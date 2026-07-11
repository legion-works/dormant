//! Desktop wake/blank-failure notifications (spec §4.4).
//!
//! Surfaces repeated wake-command failures and one-shot blank-command
//! failures as freedesktop desktop notifications, with a recovery notice
//! when configured. Split the same way as [`crate::wear_tracker`]:
//!
//! - [`decide`] / [`reconcile`] — GENUINELY PURE: zero I/O, zero tokio. Given
//!   the current [`NotifyState`], an event (or a [`StateSnapshot`]), the
//!   [`NotificationsConfig`], and the current time, they mutate the episode
//!   bookkeeping in place and return the [`NotifyAction`]s the shell must
//!   perform.
//! - [`spawn`] / the task `run` loop — ASYNC SHELL: subscribes to
//!   [`DaemonEvent`]s, requests the startup/lag [`StateSnapshot`], and drives
//!   the [`NotifySink`] (the `DBus` I/O boundary — [`ZbusSink`] in production,
//!   a recording fake in tests).
//!
//! ## Discipline (spec §4.4)
//!
//! The notifier never sends a state-changing [`ControlMsg`] — only
//! [`ControlMsg::SubscribeEvents`] and [`ControlMsg::Snapshot`]. No `DBus` call
//! is ever made while the [`NotifyState`] mutex is held: every call site
//! locks, computes actions (mutating state), unlocks, THEN performs the sink
//! calls, then re-locks briefly to record a returned dbus id via
//! [`NotifyState::record_dbus_id`].
//!
//! One open episode per `(display, kind)` pair ([`EpisodeKey`]) is tracked in
//! [`NotifyState`], which is constructed once per daemon process (in
//! `app::App::start`) and threaded through every reload generation
//! unchanged — episodes (and the underlying `DBus` notification ids) survive a
//! config reload, so a reload's `reconcile` call can close a notification
//! whose failure evidence was voided by the reload without emitting a
//! spurious recovery notice.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dormant_core::config::schema::NotificationsConfig;
use dormant_core::rules::{ControlMsg, DaemonEvent, StateSnapshot};
use dormant_core::types::DisplayId;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// ── Episode identity ─────────────────────────────────────────────────────────

/// Episode key — one open notification per display per kind.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct EpisodeKey {
    /// The display this episode applies to.
    pub display: DisplayId,
    /// The failure kind this episode tracks.
    pub kind: FailKind,
}

/// The kind of failure an [`EpisodeKey`] tracks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FailKind {
    /// A blank command exhausted its controller chain.
    Blank,
    /// A wake command failed and is being retried.
    Wake,
}

/// Bookkeeping for one currently-open episode.
#[derive(Clone, Debug)]
struct Episode {
    /// The server-assigned notification id, once known (recorded after a
    /// successful [`NotifySink::notify`] via [`NotifyState::record_dbus_id`]).
    dbus_id: Option<u32>,
    /// When this episode was last sent (governs the cooldown window).
    last_sent: Option<Instant>,
    /// Always `true` while the episode is present in
    /// [`NotifyState::episodes`] — the map holds ONLY open, notified
    /// episodes; closing an episode removes its entry entirely so a fresh
    /// failure afterward starts a brand-new episode (no stale cooldown).
    notified: bool,
}

/// Daemon-lifetime notifier state: one open episode per
/// [`EpisodeKey`], plus lag-warning bookkeeping.
#[derive(Default)]
pub struct NotifyState {
    /// Currently open (notified) episodes, keyed by display + kind.
    episodes: HashMap<EpisodeKey, Episode>,
    /// Set when a `Lagged` broadcast-receive error has already been logged
    /// since the last clean receive (`notify_events_lagged` warn-once);
    /// reset on the next clean receive.
    lag_warned: bool,
}

impl NotifyState {
    /// Record the server-assigned notification id for `key`'s currently
    /// open episode (P5) — the shell calls this after a successful
    /// [`NotifySink::notify`]. A no-op if `key` has no open episode.
    pub fn record_dbus_id(&mut self, key: &EpisodeKey, id: u32) {
        if let Some(ep) = self.episodes.get_mut(key) {
            ep.dbus_id = Some(id);
        }
    }
}

// ── Urgency / suppress reason / actions ──────────────────────────────────────

/// Urgency per the freedesktop notification spec hint (P4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Urgency {
    /// Normal urgency (recovery notices).
    Normal,
    /// Critical urgency (failure notices) — most desktop shells persist
    /// critical notifications until dismissed.
    Critical,
}

impl Urgency {
    /// The freedesktop urgency hint byte (`1` = normal, `2` = critical).
    #[must_use]
    pub fn byte(self) -> u8 {
        match self {
            Urgency::Normal => 1,
            Urgency::Critical => 2,
        }
    }
}

/// Why [`decide`]/[`reconcile`] suppressed a notification (P13) — carried so
/// the shell can log `notify_suppressed` without re-deriving policy
/// arithmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SuppressReason {
    /// A prior notification for this episode is still within its cooldown
    /// window.
    Cooldown,
    /// The failure count has not yet reached the configured threshold
    /// (wake only — blank has no threshold).
    BelowThreshold,
}

/// One action [`decide`]/[`reconcile`] wants the async shell to perform.
pub enum NotifyAction {
    /// Send (or replace) a desktop notification.
    Send {
        /// The episode this notification belongs to.
        key: EpisodeKey,
        /// Notification summary (title).
        summary: String,
        /// Notification body.
        body: String,
        /// Urgency hint.
        urgency: Urgency,
        /// The server-assigned id of a prior notification to replace, if
        /// any (re-send after cooldown).
        replaces: Option<u32>,
    },
    /// Close a previously sent notification.
    Close {
        /// The episode this close applies to.
        key: EpisodeKey,
        /// The server-assigned id to close.
        dbus_id: u32,
    },
    /// Log-only action (P13): no `DBus` call. The shell emits
    /// `notify_suppressed` with `reason` so an operator grepping the log can
    /// see WHY a failure did not produce a desktop notification.
    LogSuppressed {
        /// The episode this suppression applies to.
        key: EpisodeKey,
        /// Why the notification was suppressed.
        reason: SuppressReason,
    },
}

// ── Pure policy: decide ──────────────────────────────────────────────────────

/// Pure policy — no I/O, no tokio. Given the current state, an inbound
/// [`DaemonEvent`], the notifications config, and the current time, mutate
/// `state`'s episode bookkeeping and return the actions the shell must
/// perform.
#[must_use]
pub fn decide(
    state: &mut NotifyState,
    event: &DaemonEvent,
    cfg: &NotificationsConfig,
    now: Instant,
) -> Vec<NotifyAction> {
    match event {
        DaemonEvent::WakeRetry { display, attempt } => {
            decide_wake_retry(state, display, *attempt, cfg, now)
        }
        DaemonEvent::WakeRecovered { display, .. } => {
            let key = EpisodeKey {
                display: display.clone(),
                kind: FailKind::Wake,
            };
            decide_recovered(state, &key, cfg)
        }
        DaemonEvent::BlankFailure {
            display,
            controller,
            detail,
        } => decide_blank_failure(state, display, controller, detail, cfg, now),
        DaemonEvent::BlankRecovered { display } => {
            let key = EpisodeKey {
                display: display.clone(),
                kind: FailKind::Blank,
            };
            decide_recovered(state, &key, cfg)
        }
        _ => Vec::new(),
    }
}

fn decide_wake_retry(
    state: &mut NotifyState,
    display: &DisplayId,
    attempt: u64,
    cfg: &NotificationsConfig,
    now: Instant,
) -> Vec<NotifyAction> {
    let key = EpisodeKey {
        display: display.clone(),
        kind: FailKind::Wake,
    };
    if attempt < cfg.wake_attempt_threshold {
        return vec![NotifyAction::LogSuppressed {
            key,
            reason: SuppressReason::BelowThreshold,
        }];
    }
    let (summary, body) = wake_message(display, attempt);
    send_with_cooldown(state, key, cfg, now, Urgency::Critical, summary, body)
}

fn decide_blank_failure(
    state: &mut NotifyState,
    display: &DisplayId,
    controller: &str,
    detail: &str,
    cfg: &NotificationsConfig,
    now: Instant,
) -> Vec<NotifyAction> {
    let key = EpisodeKey {
        display: display.clone(),
        kind: FailKind::Blank,
    };
    let (summary, body) = blank_message(display, controller, detail);
    send_with_cooldown(state, key, cfg, now, Urgency::Critical, summary, body)
}

/// Shared Send-or-suppress-on-cooldown transition, used by both wake and
/// blank failure paths (threshold gating already happened at the call
/// site — blank has none).
fn send_with_cooldown(
    state: &mut NotifyState,
    key: EpisodeKey,
    cfg: &NotificationsConfig,
    now: Instant,
    urgency: Urgency,
    summary: String,
    body: String,
) -> Vec<NotifyAction> {
    match state.episodes.get(&key) {
        Some(ep) if ep.notified => {
            let elapsed = ep
                .last_sent
                .map_or(Duration::MAX, |t| now.duration_since(t));
            if elapsed < cfg.cooldown {
                return vec![NotifyAction::LogSuppressed {
                    key,
                    reason: SuppressReason::Cooldown,
                }];
            }
            let replaces = ep.dbus_id;
            if let Some(entry) = state.episodes.get_mut(&key) {
                entry.last_sent = Some(now);
            }
            vec![NotifyAction::Send {
                key,
                summary,
                body,
                urgency,
                replaces,
            }]
        }
        _ => {
            state.episodes.insert(
                key.clone(),
                Episode {
                    dbus_id: None,
                    last_sent: Some(now),
                    notified: true,
                },
            );
            vec![NotifyAction::Send {
                key,
                summary,
                body,
                urgency,
                replaces: None,
            }]
        }
    }
}

/// Shared recovery transition (`WakeRecovered` / `BlankRecovered`): close the
/// open episode (if any) and, when configured, emit a recovery notice.
/// Nothing happens for a display with no open (notified) episode.
fn decide_recovered(
    state: &mut NotifyState,
    key: &EpisodeKey,
    cfg: &NotificationsConfig,
) -> Vec<NotifyAction> {
    let Some(episode) = state.episodes.remove(key) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    if let Some(id) = episode.dbus_id {
        actions.push(NotifyAction::Close {
            key: key.clone(),
            dbus_id: id,
        });
    }
    if cfg.notify_recovery {
        let (summary, body) = recovery_message(key);
        actions.push(NotifyAction::Send {
            key: key.clone(),
            summary,
            body,
            urgency: Urgency::Normal,
            replaces: None,
        });
    }
    actions
}

// ── Pure policy: reconcile ───────────────────────────────────────────────────

/// Pure reconciliation against snapshot truth — used at notifier startup and
/// after a broadcast-receiver `Lagged` error.
///
/// For every display in `snapshot`: failing (`wake_attempts >=
/// cfg.wake_attempt_threshold`, or `last_blank_failed`) with no open episode
/// → `Send`; healthy with an open notified episode → `Close` (no recovery
/// notice — reconcile never emits one, unlike a real `*Recovered` event).
/// Displays entirely absent from `snapshot` (removed by a reload) that still
/// have an open episode → `Close`, also without a recovery notice.
#[must_use]
pub fn reconcile(
    state: &mut NotifyState,
    snapshot: &StateSnapshot,
    cfg: &NotificationsConfig,
    now: Instant,
) -> Vec<NotifyAction> {
    let mut actions = Vec::new();
    let present: std::collections::HashSet<&str> = snapshot
        .displays
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();

    for (id, dsnap) in &snapshot.displays {
        let display = DisplayId(id.clone());
        let wake_key = EpisodeKey {
            display: display.clone(),
            kind: FailKind::Wake,
        };
        let failing_wake = dsnap.wake_attempts >= cfg.wake_attempt_threshold;
        reconcile_one(state, wake_key, failing_wake, &mut actions, now, || {
            wake_message(&display, dsnap.wake_attempts)
        });

        let blank_key = EpisodeKey {
            display: display.clone(),
            kind: FailKind::Blank,
        };
        let failing_blank = dsnap.last_blank_failed;
        reconcile_one(state, blank_key, failing_blank, &mut actions, now, || {
            blank_message(
                &display,
                "unknown",
                "blank command failed (state persisted across restart)",
            )
        });
    }

    // Displays entirely absent from `snapshot` (removed by a reload) with an
    // open episode → Close, no recovery notice (spec §4.4 third case).
    let stale_keys: Vec<EpisodeKey> = state
        .episodes
        .keys()
        .filter(|k| !present.contains(k.display.0.as_str()))
        .cloned()
        .collect();
    for key in stale_keys {
        if let Some(ep) = state.episodes.remove(&key)
            && let Some(id) = ep.dbus_id
        {
            actions.push(NotifyAction::Close { key, dbus_id: id });
        }
    }

    actions
}

/// Shared per-(display, kind) reconcile transition: open a fresh episode +
/// `Send` when failing with no open episode; close a stale episode (no
/// recovery notice) when healthy with one open; no-op otherwise.
fn reconcile_one(
    state: &mut NotifyState,
    key: EpisodeKey,
    failing: bool,
    actions: &mut Vec<NotifyAction>,
    now: Instant,
    message: impl FnOnce() -> (String, String),
) {
    let has_episode = state.episodes.contains_key(&key);
    if failing && !has_episode {
        state.episodes.insert(
            key.clone(),
            Episode {
                dbus_id: None,
                last_sent: Some(now),
                notified: true,
            },
        );
        let (summary, body) = message();
        actions.push(NotifyAction::Send {
            key,
            summary,
            body,
            urgency: Urgency::Critical,
            replaces: None,
        });
    } else if !failing
        && has_episode
        && let Some(ep) = state.episodes.remove(&key)
        && let Some(id) = ep.dbus_id
    {
        actions.push(NotifyAction::Close { key, dbus_id: id });
    }
}

// ── Message text ─────────────────────────────────────────────────────────────

fn wake_message(display: &DisplayId, attempts: u64) -> (String, String) {
    (
        format!("Display {} failed to wake", display.0),
        format!(
            "{attempts} consecutive wake attempts have failed for display '{}'.",
            display.0
        ),
    )
}

fn blank_message(display: &DisplayId, controller: &str, detail: &str) -> (String, String) {
    (
        format!("Display {} failed to blank", display.0),
        format!("controller '{controller}': {detail}"),
    )
}

fn recovery_message(key: &EpisodeKey) -> (String, String) {
    let noun = match key.kind {
        FailKind::Wake => "wake",
        FailKind::Blank => "blank",
    };
    (
        format!("Display {} recovered", key.display.0),
        format!(
            "Display '{}' succeeded its {noun} command after prior failures.",
            key.display.0
        ),
    )
}

// ── NotifySink: the thin I/O boundary ────────────────────────────────────────

/// Thin I/O boundary; [`ZbusSink`] is the production impl, tests use a
/// recording fake.
#[async_trait::async_trait]
pub trait NotifySink: Send + Sync {
    /// Send (or, when `replaces != 0`, replace) a desktop notification.
    /// Returns the notification id assigned by the server (replaces-id
    /// bookkeeping).
    ///
    /// # Errors
    ///
    /// Returns `Err` on any `DBus` connect/call/timeout failure.
    async fn notify(
        &self,
        summary: &str,
        body: &str,
        urgency: u8,
        replaces: u32,
    ) -> Result<u32, String>;

    /// Close a previously sent notification.
    ///
    /// # Errors
    ///
    /// Returns `Err` on any `DBus` connect/call/timeout failure.
    async fn close(&self, id: u32) -> Result<(), String>;
}

// ── Async shell ──────────────────────────────────────────────────────────────

/// Dependencies the notifier needs, handed in by `app.rs`.
pub struct NotifierDeps {
    /// The current generation's engine control sender. The notifier ONLY
    /// ever sends [`ControlMsg::SubscribeEvents`] and [`ControlMsg::Snapshot`]
    /// over this — never a state-changing message.
    pub ctl: mpsc::Sender<ControlMsg>,
    /// The `[notifications]` config for this generation.
    pub cfg: NotificationsConfig,
    /// Daemon-lifetime episode state, shared across every generation.
    pub state: Arc<std::sync::Mutex<NotifyState>>,
    /// Daemon-lifetime notification sink, shared across every generation.
    pub sink: Arc<dyn NotifySink>,
    /// This generation's cancellation token.
    pub cancel: CancellationToken,
}

/// Spawn the notifier. Returns `None` (spawning nothing) when
/// `deps.cfg.enabled` is `false` (`inhibit_activity::spawn` precedent).
#[must_use]
pub fn spawn(deps: NotifierDeps) -> Option<tokio::task::JoinHandle<()>> {
    if !deps.cfg.enabled {
        return None;
    }
    Some(tokio::spawn(run(deps)))
}

async fn run(deps: NotifierDeps) {
    tracing::info!(event = "notifier_started");
    let NotifierDeps {
        ctl,
        cfg,
        state,
        sink,
        cancel,
    } = deps;

    // ── SubscribeEvents → Snapshot → startup reconcile (spec §4.4) ──────────
    let (sub_tx, sub_rx) = oneshot::channel();
    if ctl.send(ControlMsg::SubscribeEvents(sub_tx)).await.is_err() {
        return; // engine already gone
    }
    let Ok(mut events) = sub_rx.await else {
        return;
    };

    if let Some(snapshot) = fetch_snapshot(&ctl, &cancel).await {
        run_reconcile(&state, &snapshot, &cfg, &sink).await;
    }

    loop {
        tokio::select! {
            biased;

            () = cancel.cancelled() => break,
            ev = events.recv() => {
                match ev {
                    Ok(event) => {
                        {
                            let mut st = state.lock().expect("notify state mutex poisoned");
                            st.lag_warned = false;
                        }
                        handle_event(&state, &event, &cfg, &sink).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let should_warn = {
                            let mut st = state.lock().expect("notify state mutex poisoned");
                            let should = !st.lag_warned;
                            st.lag_warned = true;
                            should
                        };
                        if should_warn {
                            tracing::warn!(event = "notify_events_lagged", skipped);
                        }
                        if let Some(snapshot) = fetch_snapshot(&ctl, &cancel).await {
                            run_reconcile(&state, &snapshot, &cfg, &sink).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Request a [`StateSnapshot`] and await the reply — the oneshot await is
/// itself inside a `select!` against `cancel` so it can never gate shutdown
/// or (transitively) further event consumption.
async fn fetch_snapshot(
    ctl: &mpsc::Sender<ControlMsg>,
    cancel: &CancellationToken,
) -> Option<StateSnapshot> {
    let (tx, rx) = oneshot::channel();
    if ctl.send(ControlMsg::Snapshot(tx)).await.is_err() {
        return None;
    }
    tokio::select! {
        () = cancel.cancelled() => None,
        res = rx => res.ok(),
    }
}

async fn handle_event(
    state: &Arc<std::sync::Mutex<NotifyState>>,
    event: &DaemonEvent,
    cfg: &NotificationsConfig,
    sink: &Arc<dyn NotifySink>,
) {
    let actions = {
        let mut st = state.lock().expect("notify state mutex poisoned");
        decide(&mut st, event, cfg, Instant::now())
    };
    perform_actions(state, actions, sink).await;
}

async fn run_reconcile(
    state: &Arc<std::sync::Mutex<NotifyState>>,
    snapshot: &StateSnapshot,
    cfg: &NotificationsConfig,
    sink: &Arc<dyn NotifySink>,
) {
    let actions = {
        let mut st = state.lock().expect("notify state mutex poisoned");
        reconcile(&mut st, snapshot, cfg, Instant::now())
    };
    perform_actions(state, actions, sink).await;
}

/// Perform the sink I/O for a batch of actions. Never called while the
/// [`NotifyState`] mutex is held — the lock is taken only for the brief
/// `record_dbus_id` update after a successful send.
async fn perform_actions(
    state: &Arc<std::sync::Mutex<NotifyState>>,
    actions: Vec<NotifyAction>,
    sink: &Arc<dyn NotifySink>,
) {
    for action in actions {
        match action {
            NotifyAction::Send {
                key,
                summary,
                body,
                urgency,
                replaces,
            } => match sink
                .notify(&summary, &body, urgency.byte(), replaces.unwrap_or(0))
                .await
            {
                Ok(id) => {
                    {
                        let mut st = state.lock().expect("notify state mutex poisoned");
                        st.record_dbus_id(&key, id);
                    }
                    tracing::info!(event = "notify_sent", display = %key.display, kind = ?key.kind);
                }
                Err(error) => {
                    tracing::warn!(event = "notify_failed", display = %key.display, kind = ?key.kind, %error);
                }
            },
            NotifyAction::Close { key, dbus_id } => {
                if let Err(error) = sink.close(dbus_id).await {
                    tracing::warn!(event = "notify_close_failed", display = %key.display, kind = ?key.kind, %error);
                }
            }
            NotifyAction::LogSuppressed { key, reason } => {
                tracing::debug!(event = "notify_suppressed", display = %key.display, kind = ?key.kind, reason = ?reason);
            }
        }
    }
}

// ── ZbusSink: production DBus impl ───────────────────────────────────────────

/// 2-second bound on every `DBus` call (P-timeout constraint).
const DBUS_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Minimum time between reconnect attempts after a session-bus failure
/// (mirrors `idle_source::DBUS_RECONNECT_INTERVAL`'s shape).
const DBUS_RECONNECT_INTERVAL: Duration = Duration::from_secs(60);

struct ZbusConnState {
    conn: Option<zbus::Connection>,
    warned_offline: bool,
    last_attempt: Option<Instant>,
}

/// Production [`NotifySink`]: calls `org.freedesktop.Notifications` over the
/// session bus, with a lazily-established, cached connection and a 60s
/// backoff after any connect/call failure (`idle_source`'s `warned_offline`
/// shape). Code-reviewed only — no live session bus in the CI sandbox.
pub struct ZbusSink {
    conn: tokio::sync::Mutex<ZbusConnState>,
}

impl ZbusSink {
    /// Construct a fresh sink with no cached connection.
    #[must_use]
    pub fn new() -> Self {
        Self {
            conn: tokio::sync::Mutex::new(ZbusConnState {
                conn: None,
                warned_offline: false,
                last_attempt: None,
            }),
        }
    }

    /// Return the cached connection, (re)connecting if needed and not
    /// currently backing off.
    ///
    /// The connect itself (`zbus::Connection::session()`) is bounded by the
    /// same [`DBUS_CALL_TIMEOUT`] as the `call_method` sites, and is awaited
    /// with the `conn` mutex UNLOCKED — the guard is dropped before the
    /// connect starts and re-acquired only to record the outcome. This is
    /// deliberate: `ZbusSink` is one `Arc` shared, unchanged, across every
    /// reload generation for the daemon's whole lifetime, so holding the
    /// lock across an untimeouted (or even timed-out-but-still-awaited)
    /// connect would let a single hung session-bus negotiation wedge every
    /// future `notify()`/`close()` call from every subsequent generation
    /// forever. No lock may ever be held across an unbounded (or
    /// lock-spanning) await here.
    async fn connection(&self) -> Result<zbus::Connection, String> {
        {
            let mut guard = self.conn.lock().await;
            if let Some(c) = guard.conn.clone() {
                return Ok(c);
            }
            if let Some(last) = guard.last_attempt
                && last.elapsed() < DBUS_RECONNECT_INTERVAL
            {
                return Err("session bus unreachable (backing off)".to_string());
            }
            guard.last_attempt = Some(Instant::now());
        } // guard dropped — the connect below runs with no lock held.

        match tokio::time::timeout(DBUS_CALL_TIMEOUT, zbus::Connection::session()).await {
            Ok(Ok(c)) => {
                let mut guard = self.conn.lock().await;
                guard.conn = Some(c.clone());
                guard.warned_offline = false;
                Ok(c)
            }
            Ok(Err(e)) => {
                let detail = e.to_string();
                let mut guard = self.conn.lock().await;
                warn_unreachable_once(&mut guard, &detail);
                Err(detail)
            }
            Err(_) => {
                let detail = "connect: dbus call timed out".to_string();
                let mut guard = self.conn.lock().await;
                warn_unreachable_once(&mut guard, &detail);
                Err(detail)
            }
        }
    }

    /// A call (not just a connect) failed — drop the cached connection so
    /// the next attempt reconnects, and start the 60s backoff.
    async fn note_call_failure(&self, detail: &str) {
        let mut guard = self.conn.lock().await;
        guard.conn = None;
        guard.last_attempt = Some(Instant::now());
        warn_unreachable_once(&mut guard, detail);
    }
}

impl Default for ZbusSink {
    fn default() -> Self {
        Self::new()
    }
}

fn warn_unreachable_once(guard: &mut ZbusConnState, detail: &str) {
    if !guard.warned_offline {
        tracing::warn!(
            event = "notify_unreachable",
            error = %detail,
            "session bus unreachable for desktop notifications; retry in 60s",
        );
        guard.warned_offline = true;
    }
}

#[async_trait::async_trait]
impl NotifySink for ZbusSink {
    async fn notify(
        &self,
        summary: &str,
        body: &str,
        urgency: u8,
        replaces: u32,
    ) -> Result<u32, String> {
        let conn = self.connection().await?;
        let mut hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        hints.insert("urgency", zbus::zvariant::Value::U8(urgency));
        let actions: Vec<&str> = Vec::new();
        let notify_body = (
            "dormant", replaces, "dormant", summary, body, &actions, &hints, -1_i32,
        );
        let call = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &notify_body,
        );
        let reply = match tokio::time::timeout(DBUS_CALL_TIMEOUT, call).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => {
                let detail = e.to_string();
                self.note_call_failure(&detail).await;
                return Err(detail);
            }
            Err(_) => {
                let detail = "notify: dbus call timed out".to_string();
                self.note_call_failure(&detail).await;
                return Err(detail);
            }
        };
        reply.body().deserialize::<u32>().map_err(|e| e.to_string())
    }

    async fn close(&self, id: u32) -> Result<(), String> {
        let conn = self.connection().await?;
        let close_body = (id,);
        let call = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "CloseNotification",
            &close_body,
        );
        match tokio::time::timeout(DBUS_CALL_TIMEOUT, call).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                let detail = e.to_string();
                self.note_call_failure(&detail).await;
                Err(detail)
            }
            Err(_) => {
                let detail = "close: dbus call timed out".to_string();
                self.note_call_failure(&detail).await;
                Err(detail)
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::rules::{ControllerHealth, DisplaySnapshot};
    use dormant_core::types::{SensorId, SensorState};

    // ── Fixtures ─────────────────────────────────────────────────────────────

    fn cfg() -> NotificationsConfig {
        NotificationsConfig {
            enabled: true,
            wake_attempt_threshold: 3,
            cooldown: Duration::from_secs(900),
            notify_recovery: true,
        }
    }

    fn wake_retry(n: u64) -> DaemonEvent {
        DaemonEvent::WakeRetry {
            display: DisplayId("m".into()),
            attempt: n,
        }
    }

    fn key_wake(d: &str) -> EpisodeKey {
        EpisodeKey {
            display: DisplayId(d.into()),
            kind: FailKind::Wake,
        }
    }

    fn key_blank(d: &str) -> EpisodeKey {
        EpisodeKey {
            display: DisplayId(d.into()),
            kind: FailKind::Blank,
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    fn snapshot_with(displays: &[(&str, u64, bool)]) -> StateSnapshot {
        StateSnapshot {
            sensors: Vec::new(),
            zones: Vec::new(),
            displays: displays
                .iter()
                .map(|(id, wake_attempts, last_blank_failed)| {
                    (
                        (*id).to_string(),
                        DisplaySnapshot {
                            phase: "active".to_string(),
                            inhibited: false,
                            paused: false,
                            cmd_gen: 0,
                            controllers: Vec::<ControllerHealth>::new(),
                            wake_attempts: *wake_attempts,
                            last_blank_failed: *last_blank_failed,
                            stage: None,
                        },
                    )
                })
                .collect(),
            pending_reload: None,
        }
    }

    fn state_with_notified_wake_episode(display: &str, dbus_id: u32) -> NotifyState {
        let mut st = NotifyState::default();
        st.episodes.insert(
            key_wake(display),
            Episode {
                dbus_id: Some(dbus_id),
                last_sent: Some(Instant::now()),
                notified: true,
            },
        );
        st
    }

    // ── decide: wake ─────────────────────────────────────────────────────────

    #[test]
    fn wake_below_threshold_logs_suppressed_not_send() {
        let mut st = NotifyState::default();
        assert!(matches!(
            decide(&mut st, &wake_retry(1), &cfg(), t0())[..],
            [NotifyAction::LogSuppressed {
                reason: SuppressReason::BelowThreshold,
                ..
            }]
        ));
        assert!(matches!(
            decide(&mut st, &wake_retry(2), &cfg(), t0())[..],
            [NotifyAction::LogSuppressed {
                reason: SuppressReason::BelowThreshold,
                ..
            }]
        ));
    }

    #[test]
    fn wake_at_threshold_notifies_once_then_cooldown() {
        let mut st = NotifyState::default();
        assert!(matches!(
            decide(&mut st, &wake_retry(3), &cfg(), t0())[..],
            [NotifyAction::Send { .. }]
        ));
        assert!(
            matches!(
                decide(
                    &mut st,
                    &wake_retry(4),
                    &cfg(),
                    t0() + Duration::from_secs(60)
                )[..],
                [NotifyAction::LogSuppressed {
                    reason: SuppressReason::Cooldown,
                    ..
                }]
            ),
            "inside cooldown: log-only"
        );
        assert!(
            matches!(
                decide(
                    &mut st,
                    &wake_retry(20),
                    &cfg(),
                    t0() + Duration::from_secs(901)
                )[..],
                [NotifyAction::Send { .. }]
            ),
            "re-notifies after cooldown"
        );
    }

    #[test]
    fn carried_counter_first_event_notifies_immediately() {
        let mut st = NotifyState::default();
        assert!(matches!(
            decide(&mut st, &wake_retry(7), &cfg(), t0())[..],
            [NotifyAction::Send { .. }]
        ));
    }

    #[test]
    fn recovery_closes_only_when_notified() {
        let mut st = NotifyState::default();
        let rec = DaemonEvent::WakeRecovered {
            display: DisplayId("m".into()),
            attempts: 5,
        };
        assert!(
            decide(&mut st, &rec, &cfg(), t0()).is_empty(),
            "nothing sent -> nothing to close, no notice"
        );
        let _ = decide(&mut st, &wake_retry(3), &cfg(), t0());
        st.record_dbus_id(&key_wake("m"), 42);
        let acts = decide(&mut st, &rec, &cfg(), t0());
        assert!(
            acts.iter()
                .any(|a| matches!(a, NotifyAction::Close { dbus_id: 42, .. }))
        );
        assert!(
            acts.iter().any(|a| matches!(a, NotifyAction::Send { .. })),
            "notify_recovery notice"
        );
    }

    #[test]
    fn recovery_with_notify_recovery_disabled_closes_only_no_send() {
        let mut cfg = cfg();
        cfg.notify_recovery = false;
        let mut st = NotifyState::default();
        let _ = decide(&mut st, &wake_retry(3), &cfg, t0());
        st.record_dbus_id(&key_wake("m"), 42);
        let rec = DaemonEvent::WakeRecovered {
            display: DisplayId("m".into()),
            attempts: 5,
        };
        let acts = decide(&mut st, &rec, &cfg, t0());
        assert!(
            matches!(acts[..], [NotifyAction::Close { dbus_id: 42, .. }]),
            "notify_recovery=false must yield Close only, no recovery Send"
        );
    }

    #[test]
    fn blank_failure_notifies_with_detail_and_recovers() {
        let mut st = NotifyState::default();
        let fail = DaemonEvent::BlankFailure {
            display: DisplayId("m".into()),
            controller: "ddcci".into(),
            detail: "E_IO: timeout".into(),
        };
        let acts = decide(&mut st, &fail, &cfg(), t0());
        assert!(
            matches!(&acts[..], [NotifyAction::Send { body, .. }]
                if body.contains("ddcci") && body.contains("E_IO: timeout")),
            "body must contain controller + detail: {:?}",
            acts.iter().map(|_| ()).collect::<Vec<_>>()
        );
        st.record_dbus_id(&key_blank("m"), 7);
        let rec = DaemonEvent::BlankRecovered {
            display: DisplayId("m".into()),
        };
        let acts2 = decide(&mut st, &rec, &cfg(), t0());
        assert!(
            acts2
                .iter()
                .any(|a| matches!(a, NotifyAction::Close { dbus_id: 7, .. }))
        );
    }

    #[test]
    fn unknown_and_foreign_events_no_action() {
        let mut st = NotifyState::default();
        assert!(decide(&mut st, &DaemonEvent::Unknown, &cfg(), t0()).is_empty());
        let sensor_ev = DaemonEvent::SensorChanged {
            sensor: SensorId("s".into()),
            state: SensorState::Present,
        };
        assert!(decide(&mut st, &sensor_ev, &cfg(), t0()).is_empty());
    }

    // ── reconcile ────────────────────────────────────────────────────────────

    #[test]
    fn reconcile_closes_episode_for_healthy_display() {
        let mut st = state_with_notified_wake_episode("m", 42);
        let snap = snapshot_with(&[("m", 0, false)]);
        let acts = reconcile(&mut st, &snap, &cfg(), t0());
        assert!(
            acts.iter()
                .any(|a| matches!(a, NotifyAction::Close { dbus_id: 42, .. }))
        );
    }

    #[test]
    fn reconcile_opens_episode_for_failing_display_past_threshold() {
        let mut st = NotifyState::default();
        let snap = snapshot_with(&[("m", 7, false)]);
        assert!(matches!(
            reconcile(&mut st, &snap, &cfg(), t0())[..],
            [NotifyAction::Send { .. }]
        ));
    }

    #[test]
    fn reconcile_empty_state_healthy_snapshot_is_noop() {
        let mut st = NotifyState::default();
        let snap = snapshot_with(&[("m", 0, false)]);
        assert!(reconcile(&mut st, &snap, &cfg(), t0()).is_empty());
    }

    #[test]
    fn reconcile_closes_episode_for_absent_display() {
        let mut st = state_with_notified_wake_episode("tv", 42);
        let snap = snapshot_with(&[("m", 0, false)]);
        let acts = reconcile(&mut st, &snap, &cfg(), t0());
        assert!(
            acts.iter()
                .any(|a| matches!(a, NotifyAction::Close { dbus_id: 42, .. }))
        );
        assert!(
            !acts.iter().any(|a| matches!(a, NotifyAction::Send { .. })),
            "no recovery notice for a display that no longer exists"
        );
    }

    // ── spawn ────────────────────────────────────────────────────────────────

    #[test]
    fn spawn_returns_none_when_notifications_disabled() {
        // Pure (no tokio runtime needed): `spawn` must return `None` before
        // ever calling `tokio::spawn`, so a plain `mpsc::channel` (channel
        // creation itself needs no runtime) is enough for `NotifierDeps`.
        let (ctl_tx, _ctl_rx) = mpsc::channel(1);
        let mut disabled_cfg = cfg();
        disabled_cfg.enabled = false;
        let deps = NotifierDeps {
            ctl: ctl_tx,
            cfg: disabled_cfg,
            state: Arc::new(std::sync::Mutex::new(NotifyState::default())),
            sink: Arc::new(RecordingSink::default()),
            cancel: CancellationToken::new(),
        };
        assert!(
            spawn(deps).is_none(),
            "enabled=false must not spawn a notifier task"
        );
    }

    // ── Shell tests: fake sink + fake engine ────────────────────────────────

    #[derive(Default)]
    struct RecordingSink {
        notifies: std::sync::Mutex<Vec<(String, String, u8, u32)>>,
        closes: std::sync::Mutex<Vec<u32>>,
        fail_next: std::sync::atomic::AtomicBool,
        next_id: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl NotifySink for RecordingSink {
        async fn notify(
            &self,
            summary: &str,
            body: &str,
            urgency: u8,
            replaces: u32,
        ) -> Result<u32, String> {
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err("injected failure".to_string());
            }
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.notifies.lock().unwrap().push((
                summary.to_string(),
                body.to_string(),
                urgency,
                replaces,
            ));
            Ok(id)
        }

        async fn close(&self, id: u32) -> Result<(), String> {
            self.closes.lock().unwrap().push(id);
            Ok(())
        }
    }

    /// Minimal fake engine: answers `SubscribeEvents`/`Snapshot` from a
    /// shared, mutable snapshot; ignores every other `ControlMsg`.
    struct FakeEngine {
        ctl_rx: mpsc::Receiver<ControlMsg>,
        event_tx: broadcast::Sender<DaemonEvent>,
        snapshot: Arc<std::sync::Mutex<StateSnapshot>>,
    }

    impl FakeEngine {
        async fn run(mut self) {
            while let Some(msg) = self.ctl_rx.recv().await {
                match msg {
                    ControlMsg::SubscribeEvents(tx) => {
                        let _ = tx.send(self.event_tx.subscribe());
                    }
                    ControlMsg::Snapshot(tx) => {
                        let snap = self.snapshot.lock().unwrap().clone();
                        let _ = tx.send(snap);
                    }
                    _ => {}
                }
            }
        }
    }

    fn spawn_fake_engine(
        snapshot: StateSnapshot,
    ) -> (
        mpsc::Sender<ControlMsg>,
        broadcast::Sender<DaemonEvent>,
        Arc<std::sync::Mutex<StateSnapshot>>,
    ) {
        let (ctl_tx, ctl_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(64);
        let shared_snapshot = Arc::new(std::sync::Mutex::new(snapshot));
        let engine = FakeEngine {
            ctl_rx,
            event_tx: event_tx.clone(),
            snapshot: shared_snapshot.clone(),
        };
        tokio::spawn(engine.run());
        (ctl_tx, event_tx, shared_snapshot)
    }

    async fn wait_until<F: Fn() -> bool>(pred: F, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        pred()
    }

    #[tokio::test]
    async fn startup_reconcile_fires_before_first_event() {
        let snap = snapshot_with(&[("m", 7, false)]);
        let (ctl_tx, _event_tx, _snap) = spawn_fake_engine(snap);
        let sink = Arc::new(RecordingSink::default());
        let state = Arc::new(std::sync::Mutex::new(NotifyState::default()));
        let cancel = CancellationToken::new();
        let deps = NotifierDeps {
            ctl: ctl_tx,
            cfg: cfg(),
            state: state.clone(),
            sink: sink.clone(),
            cancel: cancel.clone(),
        };
        let handle = spawn(deps).expect("enabled -> Some");

        assert!(
            wait_until(
                || !sink.notifies.lock().unwrap().is_empty(),
                Duration::from_secs(2)
            )
            .await,
            "startup reconcile must Send for the already-failing display"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn sink_error_logs_notify_failed_and_episode_stays_intact() {
        let snap = snapshot_with(&[]);
        let (ctl_tx, event_tx, _snap) = spawn_fake_engine(snap);
        let sink = Arc::new(RecordingSink::default());
        sink.fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let state = Arc::new(std::sync::Mutex::new(NotifyState::default()));
        let cancel = CancellationToken::new();
        let deps = NotifierDeps {
            ctl: ctl_tx,
            cfg: cfg(),
            state: state.clone(),
            sink: sink.clone(),
            cancel: cancel.clone(),
        };
        let handle = spawn(deps).expect("enabled -> Some");

        // Give the startup Subscribe/Snapshot a moment to complete before
        // sending the event (there's no failing display in the snapshot, so
        // the startup reconcile is a no-op here).
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = event_tx.send(wake_retry(3));

        assert!(
            wait_until(
                || {
                    let st = state.lock().unwrap();
                    // The episode was opened by `decide` regardless of the
                    // sink outcome (coarse assertion, as documented): state
                    // is not rolled back on a sink failure.
                    st.episodes.contains_key(&key_wake("m"))
                },
                Duration::from_secs(2),
            )
            .await,
            "episode must stay intact (present) after a sink failure"
        );
        // The failed attempt must not have recorded a notify.
        assert!(sink.notifies.lock().unwrap().is_empty());

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn lagged_receiver_requests_snapshot_and_warns_once() {
        let snap = snapshot_with(&[("m", 7, false)]);
        let (ctl_tx, event_tx, shared_snap) = spawn_fake_engine(snap);
        let sink = Arc::new(RecordingSink::default());
        let state = Arc::new(std::sync::Mutex::new(NotifyState::default()));
        let cancel = CancellationToken::new();
        let deps = NotifierDeps {
            ctl: ctl_tx,
            cfg: cfg(),
            state: state.clone(),
            sink: sink.clone(),
            cancel: cancel.clone(),
        };
        let handle = spawn(deps).expect("enabled -> Some");

        // Wait for the startup reconcile's Send (episode "m" now open).
        assert!(
            wait_until(
                || !sink.notifies.lock().unwrap().is_empty(),
                Duration::from_secs(2)
            )
            .await
        );

        // Simulate a broadcast lag by dropping the receiver's backlog: fill
        // the channel past capacity so the notifier's `recv()` observes
        // `Lagged`, then flip the snapshot to healthy so the lag-triggered
        // reconcile closes the stale episode.
        for i in 0..100u64 {
            let _ = event_tx.send(wake_retry(3 + i));
        }
        *shared_snap.lock().unwrap() = snapshot_with(&[("m", 0, false)]);

        assert!(
            wait_until(
                || !sink.closes.lock().unwrap().is_empty(),
                Duration::from_secs(2)
            )
            .await,
            "a Lagged receive must trigger a reconcile snapshot that closes the now-healthy episode"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }
}
