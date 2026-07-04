//! User-activity inhibitor.
//!
//! Polls `org.freedesktop.ScreenSaver.GetSessionIdleTime` on the session bus
//! and publishes rule-level inhibition into the rules engine via
//! [`ControlMsg::SetInhibited`]. One poller runs at the minimum of the
//! declaring rules' `activity_poll_interval`; each rule is evaluated against
//! its own `activity_idle_threshold`.
//!
//! ## Fail-toward-normal-blanking
//!
//! If the session bus is unreachable (no compositor screensaver service, or
//! the call errors) the inhibitor treats the user as **inactive** — it emits
//! `inhibited = false` rather than holding screens on forever. A broken idle
//! probe must never wedge displays awake: the sensor/zone layer still guards
//! actual presence. The connection is retried every 60s and the unreachable
//! condition is warned once.
//!
//! The `DBus` reply is interpreted as **milliseconds** of idle time.

use std::collections::HashMap;
use std::time::Duration;

use dormant_core::rules::ControlMsg;
use dormant_core::types::RuleId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Reconnect interval after a session-bus failure.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(60);

/// One rule that declares the `user-activity` inhibitor, with its idle
/// threshold.
#[derive(Debug, Clone)]
pub struct ActivityRule {
    /// The rule this inhibitor gates.
    pub rule: RuleId,
    /// Idle time below which the user is considered active (inhibited).
    pub idle_threshold: Duration,
}

/// Spawn the activity-inhibitor poller.
///
/// Returns `None` (spawning nothing) when no rule declares `user-activity`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn spawn(
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if rules.is_empty() {
        return None;
    }
    Some(tokio::spawn(run(rules, poll_interval, ctl, cancel)))
}

/// Non-Linux stub — there is no session-bus screensaver service to poll.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn spawn(
    _rules: Vec<ActivityRule>,
    _poll_interval: Duration,
    _ctl: mpsc::Sender<ControlMsg>,
    _cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    None
}

#[cfg(target_os = "linux")]
async fn run(
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    let mut conn: Option<zbus::Connection> = None;
    let mut last_sent: HashMap<RuleId, bool> = HashMap::new();
    let mut warned = false;

    loop {
        // Establish (or re-establish) the session-bus connection.
        if conn.is_none() {
            match zbus::Connection::session().await {
                Ok(c) => {
                    conn = Some(c);
                    warned = false;
                }
                Err(e) => {
                    if !warned {
                        tracing::warn!(
                            event = "activity_inhibitor_unreachable",
                            error = %e,
                            "session bus unreachable; treating user as inactive \
                             (fail toward normal blanking), retrying in 60s",
                        );
                        warned = true;
                    }
                    // Fail toward normal blanking: nobody is holding screens on.
                    for r in &rules {
                        publish(&ctl, &mut last_sent, &r.rule, false);
                    }
                    if sleep_or_cancel(RECONNECT_INTERVAL, &cancel).await {
                        return;
                    }
                    continue;
                }
            }
        }

        match get_idle_ms(conn.as_ref().expect("connection present")).await {
            Ok(idle) => {
                let idle = Duration::from_millis(idle);
                for r in &rules {
                    let inhibited = idle < r.idle_threshold;
                    publish(&ctl, &mut last_sent, &r.rule, inhibited);
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "activity_inhibitor_probe_failed",
                    error = %e,
                    "idle probe failed; dropping connection and treating user as inactive",
                );
                conn = None;
                for r in &rules {
                    publish(&ctl, &mut last_sent, &r.rule, false);
                }
            }
        }

        if sleep_or_cancel(poll_interval, &cancel).await {
            return;
        }
    }
}

/// Send a `SetInhibited` only when the value changed for this rule.
#[cfg(target_os = "linux")]
fn publish(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<RuleId, bool>,
    rule: &RuleId,
    inhibited: bool,
) {
    if last_sent.get(rule) == Some(&inhibited) {
        return;
    }
    last_sent.insert(rule.clone(), inhibited);
    // try_send: the control channel is bounded; dropping a stale edge is
    // acceptable because the next poll re-evaluates from scratch.
    let _ = ctl.try_send(ControlMsg::SetInhibited {
        rule: Some(rule.clone()),
        inhibited,
    });
}

/// Query `GetSessionIdleTime` (milliseconds) from the screensaver service.
#[cfg(target_os = "linux")]
async fn get_idle_ms(conn: &zbus::Connection) -> zbus::Result<u64> {
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
#[cfg(target_os = "linux")]
async fn sleep_or_cancel(dur: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(dur) => false,
    }
}
