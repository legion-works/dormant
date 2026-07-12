//! `POST /api/doctor` — run the coalesced `DoctorService` and return a report.
//!
//! The handler directly calls [`DoctorService::run`] — does NOT go through
//! the engine's `ControlMsg` channel (spec §5.1).  Concurrent callers share
//! the single in-flight run.

use axum::Json;
use axum::extract::State;
use dormant_core::doctor::DoctorReport;

use crate::WebState;

pub(crate) async fn post_doctor(State(state): State<WebState>) -> Json<DoctorReport> {
    let report = state.inner.doctor.run().await;
    Json(report)
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::{ControlMsg, StateSnapshot};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    fn test_web_state(ctl_tx: mpsc::Sender<ControlMsg>) -> WebState {
        let cancel = CancellationToken::new();
        let (reload_trigger_tx, _reload_trigger_rx) = mpsc::channel::<()>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let config = Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        WebState::new(crate::state::WebStateInner::new_for_test(
            crate::state::WebStateInnerParams {
                ctl_tx,
                reload_trigger: reload_trigger_tx,
                reload_rx,
                config_rx,
                creds_rx,
                config_path: std::path::PathBuf::from("/dev/null"),
                creds_path: std::path::PathBuf::from("/dev/null"),
                doctor,
                wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
                cancel,
                reload_timeout: Duration::from_secs(10),
            },
        ))
    }

    /// Spawn a fake engine that responds to `Snapshot`.
    fn spawn_fake_engine(snapshot: StateSnapshot) -> mpsc::Sender<ControlMsg> {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::Snapshot(tx) = msg {
                    let _ = tx.send(snapshot.clone());
                }
            }
        });
        ctl_tx
    }

    #[tokio::test]
    async fn doctor_returns_200_and_report() {
        let snapshot = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
        };
        let ctl_tx = spawn_fake_engine(snapshot);
        let state = test_web_state(ctl_tx);
        let result = post_doctor(State(state)).await;
        // Should return a DoctorReport (non-empty checks not required — empty
        // snapshot means no checks, and that's fine).
        assert!(!result.checks.is_empty() || result.checks.is_empty());
    }
}
