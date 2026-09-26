//! Post-hoc wake hooks from the daemon event stream.

use std::sync::Arc;

use dormant_core::rules::{ControlMsg, DaemonEvent};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::direct_switch::DirectSwitchHandle;

fn should_fire_on_wake(_phase: &str, cause: &str, presence_confirmed: Option<bool>) -> bool {
    match cause {
        "presence_detected" => presence_confirmed == Some(true),
        "input_wake" | "force_wake" => true,
        _ => false,
    }
}

async fn handle_event(event: &DaemonEvent, direct_switch: &DirectSwitchHandle) {
    if let DaemonEvent::DisplayPhase {
        display,
        phase,
        cause,
        presence_confirmed,
    } = event
        && should_fire_on_wake(phase, cause, *presence_confirmed)
    {
        direct_switch.notify_wake(display).await;
    }
}

pub(crate) fn spawn(
    ctl: mpsc::Sender<ControlMsg>,
    direct_switch: Arc<DirectSwitchHandle>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (sub_tx, sub_rx) = oneshot::channel();
        if ctl.send(ControlMsg::SubscribeEvents(sub_tx)).await.is_err() {
            return;
        }
        let Ok(mut events) = sub_rx.await else { return };
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                event = events.recv() => match event {
                    Ok(event) => handle_event(&event, &direct_switch).await,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(event = "wake_hooks_events_lagged", skipped);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_wake_fires_on_confirmed_presence_wake() {
        assert!(should_fire_on_wake(
            "waking",
            "presence_detected",
            Some(true)
        ));
        assert!(should_fire_on_wake(
            "active",
            "presence_detected",
            Some(true)
        ));
    }

    #[test]
    fn on_wake_skips_fail_safe_presence_wake() {
        assert!(!should_fire_on_wake(
            "waking",
            "presence_detected",
            Some(false)
        ));
        assert!(!should_fire_on_wake("active", "presence_detected", None));
    }

    #[test]
    fn on_wake_fires_on_input_and_force_wakes() {
        for cause in ["input_wake", "force_wake"] {
            assert!(should_fire_on_wake("waking", cause, None), "{cause}");
            assert!(should_fire_on_wake("active", cause, None), "{cause}");
        }
    }

    #[test]
    fn on_wake_fires_on_grace_return_input_wake() {
        assert!(should_fire_on_wake("active", "input_wake", None));
    }

    #[test]
    fn on_wake_skips_ownership_acquired() {
        assert!(!should_fire_on_wake("waking", "ownership_acquired", None));
        assert!(!should_fire_on_wake(
            "active",
            "ownership_acquired",
            Some(true)
        ));
    }

    #[test]
    fn on_wake_skips_retry_completion_and_grace_return() {
        for cause in ["wake_retry", "wake_completed", "presence_during_grace"] {
            assert!(!should_fire_on_wake("waking", cause, Some(true)), "{cause}");
            assert!(!should_fire_on_wake("active", cause, Some(true)), "{cause}");
        }
        assert!(!should_fire_on_wake("blanking", "force_blank", None));
    }
}
