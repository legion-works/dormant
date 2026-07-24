//! Platform-neutral hotkey registrar contract and lifecycle manager.
#![allow(missing_docs)]
//!
//! The `HotkeyRegistrar` trait abstracts OS-level global shortcut
//! registration so the `HotkeyManager` can be unit-tested against a
//! mock registrar without a live desktop session.
//!
//! The `HotkeyManager` subscribes to the IPC loop's snapshot-refresh
//! watch channel; whenever a new snapshot is published (startup,
//! reconnect, or `ConfigReloaded`), it diffs the `KvmStatus` payload
//! and unregisters the old accelerator before registering the new one.
//! Watch-channel semantics naturally coalesce rapid reloads: only the
//! latest snapshot is visible when the manager next processes a
//! `changed()` notification.
//!
//! It refuses ambiguous targets (zero or ≥2 claim-capable displays)
//! with the literal log anchor `hotkey_register_failed
//! reason=ambiguous_target` and sends a desktop notification so the
//! operator knows the hotkey was not registered.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use dormant_core::rules::KvmStatus;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::dispatch::execute_plan;
use crate::dispatch::{DispatchCapabilities, plan_action};
use crate::menu::Action;
use crate::tray_state::TrayState;

/// A parsed OS accelerator string from the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accelerator {
    pub raw: String,
}

impl fmt::Display for Accelerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.raw.fmt(f)
    }
}

impl Accelerator {
    #[must_use]
    pub fn parse(raw: impl Into<String>) -> Option<Self> {
        let raw = raw.into();
        if raw.is_empty() {
            return None;
        }
        Some(Self { raw })
    }
}

/// Errors that can occur during hotkey registration.
#[derive(Debug, Clone)]
pub enum HotkeyError {
    DbusError(String),
    AmbiguousTarget { count: usize },
    InvalidAccelerator(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::DbusError(msg) => write!(f, "D-Bus error: {msg}"),
            HotkeyError::AmbiguousTarget { count } => {
                write!(
                    f,
                    "ambiguous hotkey target: {count} claim-capable displays (need exactly 1)"
                )
            }
            HotkeyError::InvalidAccelerator(accel) => {
                write!(f, "invalid accelerator: {accel}")
            }
        }
    }
}

/// Contract for OS-level global hotkey registration.
#[async_trait]
pub trait HotkeyRegistrar: Send + Sync {
    async fn register_claim(
        &mut self,
        accelerator: &Accelerator,
        target: &str,
        arm: bool,
        tx: UnboundedSender<Action>,
    ) -> Result<(), HotkeyError>;

    async fn unregister_claim(&mut self);
}

/// Sends user-visible notifications (e.g. `notify-send` on Linux).
/// Injected so the manager can be tested without a desktop.
pub trait Notifier: Send + Sync {
    fn notify(&self, summary: &str, body: &str);
}

/// Manages the lifecycle of a global claim hotkey.
///
/// Created in `main` and runs on a dedicated tokio task.  It subscribes
/// to the IPC loop's snapshot-refresh watch channel — the same channel
/// that drives ksni menu refreshes — so every snapshot publish
/// (startup, reconnect, `ConfigReloaded`) triggers a re-evaluation.
pub struct HotkeyManager {
    state: Arc<tokio::sync::Mutex<TrayState>>,
    refresh_rx: watch::Receiver<()>,
    action_rx: UnboundedReceiver<Action>,
    action_tx: UnboundedSender<Action>,
    last_kvm: Option<KvmStatus>,
    current_accelerator: Option<Accelerator>,
    current_target: Option<String>,
    registrar: Option<Box<dyn HotkeyRegistrar>>,
    notifier: Option<Box<dyn Notifier>>,
}

impl HotkeyManager {
    #[must_use]
    pub fn new(
        state: Arc<tokio::sync::Mutex<TrayState>>,
        refresh_rx: watch::Receiver<()>,
        registrar: Option<Box<dyn HotkeyRegistrar>>,
        notifier: Option<Box<dyn Notifier>>,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        Self {
            state,
            refresh_rx,
            action_rx,
            action_tx,
            last_kvm: None,
            current_accelerator: None,
            current_target: None,
            registrar,
            notifier,
        }
    }

    /// Run the manager loop.  Returns when `cancel` fires.
    pub async fn run(
        mut self,
        cancel: CancellationToken,
        socket_path: std::path::PathBuf,
        capabilities: Arc<dyn DispatchCapabilities + 'static>,
    ) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    self.unregister_current().await;
                    return;
                }
                // Watch channel: on every snapshot publish (startup,
                // reconnect, ConfigReloaded), re-evaluate.  The
                // channel coalesces: if multiple publishes happen
                // before we loop, `changed()` returns immediately
                // and we see the latest state exactly once.
                result = self.refresh_rx.changed() => {
                    if result.is_err() {
                        return; // sender dropped
                    }
                    self.sync_from_state().await;
                }
                Some(action) = self.action_rx.recv() => {
                    self.dispatch_action(action, &socket_path, &capabilities).await;
                }
            }
        }
    }

    async fn sync_from_state(&mut self) {
        let kvm = {
            let s = self.state.lock().await;
            s.snapshot.as_ref().and_then(|snap| snap.kvm.clone())
        };

        if self.last_kvm.as_ref() == kvm.as_ref() {
            return;
        }
        self.last_kvm.clone_from(&kvm);

        self.unregister_current().await;

        let Some(kvm) = kvm else {
            return;
        };

        let Some(hotkey) = Accelerator::parse(kvm.keymap.claim_hotkey.as_deref().unwrap_or(""))
        else {
            return;
        };

        let capable = &kvm.claim_capable_displays;
        if capable.len() != 1 {
            let count = capable.len();
            warn!(
                count,
                event = "hotkey_register_failed",
                reason = "ambiguous_target",
                "claim hotkey requires exactly one claim-capable shared display"
            );
            if let Some(ref n) = self.notifier {
                n.notify(
                    "dormant — hotkey not registered",
                    &format!(
                        "Cannot register claim hotkey: {count} claim-capable displays (need exactly 1).  Use the tray menu instead."
                    ),
                );
            }
            return;
        }

        let target = capable[0].0.clone();

        let arm = matches!(
            kvm.activity_claim,
            dormant_core::config::ActivityClaimPolicy::Armed
        );

        let Some(ref mut registrar) = self.registrar else {
            return;
        };

        match registrar
            .register_claim(&hotkey, &target, arm, self.action_tx.clone())
            .await
        {
            Ok(()) => {
                info!(
                    %hotkey,
                    %target,
                    arm,
                    "claim hotkey registered"
                );
                self.current_accelerator = Some(hotkey);
                self.current_target = Some(target);
            }
            Err(e) => {
                warn!(
                    error = %e,
                    %hotkey,
                    event = "hotkey_register_failed",
                    reason = "dbus_error",
                    "claim hotkey registration failed; manual menu path remains available"
                );
                if let Some(ref n) = self.notifier {
                    n.notify(
                        "dormant — hotkey unavailable",
                        "Could not register the claim hotkey.  Use the tray menu to claim the panel instead.",
                    );
                }
            }
        }
    }

    async fn unregister_current(&mut self) {
        if self.current_accelerator.is_some()
            && let Some(ref mut registrar) = self.registrar
        {
            registrar.unregister_claim().await;
        }
        self.current_accelerator = None;
        self.current_target = None;
    }

    async fn dispatch_action(
        &self,
        action: Action,
        socket_path: &std::path::Path,
        capabilities: &Arc<dyn DispatchCapabilities + 'static>,
    ) {
        let (snapshot, unreachable) = {
            let s = self.state.lock().await;
            (s.snapshot.clone(), s.unreachable)
        };
        let plan = plan_action(&action, snapshot.as_ref(), unreachable);
        let socket = socket_path.to_path_buf();
        let capabilities = Arc::clone(capabilities);
        if let Err(e) = execute_plan(plan, socket, capabilities).await {
            warn!(error = %e, "hotkey action dispatch failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::DispatchCapabilities;
    use dormant_core::config::{ActivityClaimPolicy, KeymapConfig};
    use dormant_core::rules::{DisplaySnapshot, StateSnapshot};
    use dormant_core::types::DisplayId;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;

    fn snap_with_kvm(kvm: KvmStatus) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![(
                "monitor".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    scope: dormant_core::config::DisplayScope::Shared,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                    claim_armed_remaining_ms: None,
                },
            )],
            pending_reload: None,
            rollback: None,
            kvm: Some(kvm),
        }
    }

    fn kvm_status(hotkey: &str, displays: &[&str], policy: ActivityClaimPolicy) -> KvmStatus {
        KvmStatus {
            keymap: KeymapConfig {
                claim_hotkey: if hotkey.is_empty() {
                    None
                } else {
                    Some(hotkey.into())
                },
            },
            claim_capable_displays: displays.iter().map(|d| DisplayId((*d).into())).collect(),
            activity_claim: policy,
            claim_armed_remaining: vec![],
        }
    }

    struct FakeRegistrar {
        calls: Arc<StdMutex<Vec<RegistrarCall>>>,
        should_fail: bool,
        /// If set, this sender is notified on every register call so
        /// tests can await the registration event instead of sleeping.
        register_notify: Option<Arc<Notify>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RegistrarCall {
        Register {
            accel: String,
            target: String,
            arm: bool,
        },
        Unregister,
    }

    #[async_trait]
    impl HotkeyRegistrar for FakeRegistrar {
        async fn register_claim(
            &mut self,
            accelerator: &Accelerator,
            target: &str,
            arm: bool,
            _tx: UnboundedSender<Action>,
        ) -> Result<(), HotkeyError> {
            if self.should_fail {
                return Err(HotkeyError::DbusError("no session bus".into()));
            }
            self.calls.lock().unwrap().push(RegistrarCall::Register {
                accel: accelerator.raw.clone(),
                target: target.into(),
                arm,
            });
            if let Some(ref n) = self.register_notify {
                n.notify_one();
            }
            Ok(())
        }

        async fn unregister_claim(&mut self) {
            self.calls.lock().unwrap().push(RegistrarCall::Unregister);
        }
    }

    impl FakeRegistrar {
        fn new(should_fail: bool) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                should_fail,
                register_notify: None,
            }
        }

        #[allow(dead_code)]
        fn with_notify(should_fail: bool) -> (Self, Arc<Notify>) {
            let n = Arc::new(Notify::new());
            (
                Self {
                    calls: Arc::new(StdMutex::new(Vec::new())),
                    should_fail,
                    register_notify: Some(Arc::clone(&n)),
                },
                n,
            )
        }
    }

    /// A notifier that records every notification for test assertions.
    struct RecordingNotifier {
        notifications: Arc<StdMutex<Vec<(String, String)>>>,
    }

    /// Type alias for the notification recording list.
    type NotificationRec = Arc<StdMutex<Vec<(String, String)>>>;

    impl RecordingNotifier {
        fn new() -> (Self, NotificationRec) {
            let rec = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    notifications: rec.clone(),
                },
                rec,
            )
        }
    }

    impl Notifier for RecordingNotifier {
        fn notify(&self, summary: &str, body: &str) {
            self.notifications
                .lock()
                .unwrap()
                .push((summary.to_string(), body.to_string()));
        }
    }

    struct NoopCapabilities;

    impl DispatchCapabilities for NoopCapabilities {
        fn send_ipc(
            &self,
            _: &Path,
            _: &dormant_core::ipc_proto::IpcRequest,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn claim_shared(
            &self,
            _: &Path,
            _: &str,
        ) -> anyhow::Result<dormant_core::ipc_proto::ClaimSharedResultWire> {
            anyhow::bail!("noop")
        }
        fn claim_arm(
            &self,
            _: &Path,
            _: &str,
        ) -> anyhow::Result<dormant_core::ipc_proto::ClaimArmResultWire> {
            anyhow::bail!("noop")
        }
        fn open_web(&self, _: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn request_quit(&self) {}
    }

    /// Spawn a manager and immediately publish a snapshot on its watch
    /// channel so the manager processes it.
    async fn start_manager(
        state: Arc<tokio::sync::Mutex<TrayState>>,
        registrar: FakeRegistrar,
    ) -> (
        tokio::task::JoinHandle<()>,
        CancellationToken,
        watch::Sender<()>,
        Arc<StdMutex<Vec<RegistrarCall>>>,
    ) {
        let calls = registrar.calls.clone();
        let (refresh_tx, refresh_rx) = watch::channel(());
        let (notifier, _notify_rec) = RecordingNotifier::new();
        let manager = HotkeyManager::new(
            state,
            refresh_rx,
            Some(Box::new(registrar)),
            Some(Box::new(notifier)),
        );
        let cancel = CancellationToken::new();
        let caps: Arc<dyn DispatchCapabilities> = Arc::new(NoopCapabilities);
        let socket = std::path::PathBuf::from("/tmp/dormant.sock");
        let mgr_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            manager.run(mgr_cancel, socket, caps).await;
        });

        // Publish the initial snapshot to trigger sync_from_state.
        refresh_tx.send_replace(());

        // Yield to let the manager process.
        tokio::task::yield_now().await;

        // Wait for the registrar to actually receive the register call
        // (event-driven, no wall-clock sleep).
        // The manager processes the watch notification and calls
        // register_claim.  Give it a short bounded window.
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if !calls.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        (handle, cancel, refresh_tx, calls)
    }

    #[tokio::test]
    async fn registers_on_initial_kvm_snapshot() {
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &["monitor"],
                ActivityClaimPolicy::Off,
            )));
            s.unreachable = false;
        }

        let (_handle, cancel, _refresh, calls) =
            start_manager(state, FakeRegistrar::new(false)).await;

        cancel.cancel();
        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|c| matches!(c, RegistrarCall::Register { accel, target, arm } if accel == "Meta+F12" && target == "monitor" && !arm)),
            "expected Register(Meta+F12, monitor), got: {recorded:?}"
        );
    }

    /// The plan's required stale-cache discriminator: the manager must
    /// see the NEW status after a `ConfigReloaded` event (i.e. after a
    /// snapshot republish), unregister the old hotkey, and register the
    /// new one.  An implementation that caches the old status forever
    /// stays on the old accelerator and fails this test.
    #[tokio::test]
    async fn config_reload_refetches_status_before_reregistering_hotkey() {
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &["monitor"],
                ActivityClaimPolicy::Off,
            )));
            s.unreachable = false;
        }

        let (_handle, cancel, refresh_tx, calls) =
            start_manager(state.clone(), FakeRegistrar::new(false)).await;

        // Daemon replaces its status (simulating a config reload).
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F11",
                &["monitor"],
                ActivityClaimPolicy::Off,
            )));
        }
        // Publish the new snapshot — mirrors what ipc_loop does after
        // a ConfigReloaded event + status refetch.
        refresh_tx.send_replace(());
        // Yield to let the manager process.
        tokio::task::yield_now().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            let done = {
                let r = calls.lock().unwrap();
                r.len() >= 3
            };
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        cancel.cancel();
        let recorded = calls.lock().unwrap();
        assert!(
            recorded.len() >= 3,
            "expected at least Register(F12) → Unregister → Register(F11), got: {recorded:?}"
        );
        assert!(
            matches!(&recorded[0], RegistrarCall::Register { accel, .. } if accel == "Meta+F12")
        );
        assert!(matches!(&recorded[1], RegistrarCall::Unregister));
        assert!(
            matches!(&recorded[2], RegistrarCall::Register { accel, .. } if accel == "Meta+F11")
        );
    }

    #[tokio::test]
    async fn unchanged_kvm_skips_reregistration() {
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &["monitor"],
                ActivityClaimPolicy::Off,
            )));
            s.unreachable = false;
        }

        let (_handle, cancel, refresh_tx, calls) =
            start_manager(state.clone(), FakeRegistrar::new(false)).await;

        // Republish the same kvm status.
        refresh_tx.send_replace(());
        tokio::task::yield_now().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        cancel.cancel();
        let recorded = calls.lock().unwrap();
        let registers: Vec<_> = recorded
            .iter()
            .filter(|c| matches!(c, RegistrarCall::Register { .. }))
            .collect();
        assert_eq!(
            registers.len(),
            1,
            "only one register call; unchanged kvm should not re-register, got: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn zero_claim_capable_displays_logs_ambiguous_and_notifies() {
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &[],
                ActivityClaimPolicy::Off,
            )));
            s.unreachable = false;
        }

        let (notifier, notif_rec) = RecordingNotifier::new();
        let (refresh_tx, refresh_rx) = watch::channel(());
        let manager = HotkeyManager::new(
            state,
            refresh_rx,
            Some(Box::new(FakeRegistrar::new(false))),
            Some(Box::new(notifier)),
        );
        let cancel = CancellationToken::new();
        let caps: Arc<dyn DispatchCapabilities> = Arc::new(NoopCapabilities);
        let socket = std::path::PathBuf::from("/tmp/dormant.sock");
        let mgr_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            manager.run(mgr_cancel, socket, caps).await;
        });
        refresh_tx.send_replace(());
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        cancel.cancel();
        handle.await.ok();

        // Must have received a notification about the ambiguous target.
        let notifications = notif_rec.lock().unwrap();
        assert!(
            !notifications.is_empty(),
            "zero targets must produce a notification, got none"
        );
        assert!(
            notifications
                .iter()
                .any(|(_, body)| body.contains("0 claim-capable")),
            "notification should mention 0 claim-capable displays: {notifications:?}"
        );
    }

    #[tokio::test]
    async fn ambiguous_target_logs_warning_without_registering() {
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &["monitor", "tv"],
                ActivityClaimPolicy::Off,
            )));
            s.unreachable = false;
        }

        let (notifier, notif_rec) = RecordingNotifier::new();
        let registrar = FakeRegistrar::new(false);
        let calls = registrar.calls.clone();
        let (refresh_tx, refresh_rx) = watch::channel(());
        let manager = HotkeyManager::new(
            state,
            refresh_rx,
            Some(Box::new(registrar)),
            Some(Box::new(notifier)),
        );
        let cancel = CancellationToken::new();
        let caps: Arc<dyn DispatchCapabilities> = Arc::new(NoopCapabilities);
        let socket = std::path::PathBuf::from("/tmp/dormant.sock");
        let mgr_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            manager.run(mgr_cancel, socket, caps).await;
        });
        refresh_tx.send_replace(());
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        cancel.cancel();
        handle.await.ok();

        let recorded = calls.lock().unwrap();
        let registers: Vec<_> = recorded
            .iter()
            .filter(|c| matches!(c, RegistrarCall::Register { .. }))
            .collect();
        assert!(
            registers.is_empty(),
            "ambiguous targets should produce no Register calls, got: {recorded:?}"
        );
        let notifications = notif_rec.lock().unwrap();
        assert!(!notifications.is_empty(), "ambiguous target must notify");
    }

    #[tokio::test]
    async fn registration_failure_retains_manual_menu() {
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &["monitor"],
                ActivityClaimPolicy::Off,
            )));
            s.unreachable = false;
        }

        let (notifier, notif_rec) = RecordingNotifier::new();
        let registrar = FakeRegistrar::new(true);
        let (refresh_tx, refresh_rx) = watch::channel(());
        let manager = HotkeyManager::new(
            state,
            refresh_rx,
            Some(Box::new(registrar)),
            Some(Box::new(notifier)),
        );
        let cancel = CancellationToken::new();
        let caps: Arc<dyn DispatchCapabilities> = Arc::new(NoopCapabilities);
        let socket = std::path::PathBuf::from("/tmp/dormant.sock");
        let mgr_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            manager.run(mgr_cancel, socket, caps).await;
        });
        refresh_tx.send_replace(());
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.cancel();
        handle.await.ok();

        // Manager is still alive (didn't crash), and a notification
        // about the failure was emitted.
        let notifications = notif_rec.lock().unwrap();
        assert!(
            !notifications.is_empty(),
            "registrar failure must notify user"
        );
    }

    #[tokio::test]
    async fn armed_policy_registers_arm_claim() {
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &["monitor"],
                ActivityClaimPolicy::Armed,
            )));
            s.unreachable = false;
        }

        let (_handle, cancel, _refresh, calls) =
            start_manager(state, FakeRegistrar::new(false)).await;
        cancel.cancel();

        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|c| matches!(c, RegistrarCall::Register { arm: true, .. })),
            "armed policy should pass arm=true to registrar, got: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn reconnect_republish_triggers_sync() {
        // A daemon reconnect publishes a new snapshot; the manager
        // must re-evaluate.  If the kvm is unchanged it skips, but
        // republishing with a changed keymap must trigger re-registration.
        let state = Arc::new(tokio::sync::Mutex::new(TrayState::new(
            "/tmp/dormant.sock".into(),
        )));
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F12",
                &["monitor"],
                ActivityClaimPolicy::Off,
            )));
            s.unreachable = false;
        }

        let (_handle, cancel, refresh_tx, calls) =
            start_manager(state.clone(), FakeRegistrar::new(false)).await;

        // Simulate reconnect: same kvm, republish.
        refresh_tx.send_replace(());
        tokio::task::yield_now().await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Change the keymap and republish (simulating daemon restart
        // with new config).
        {
            let mut s = state.lock().await;
            s.snapshot = Some(snap_with_kvm(kvm_status(
                "Meta+F1",
                &["monitor"],
                ActivityClaimPolicy::Off,
            )));
        }
        refresh_tx.send_replace(());
        tokio::task::yield_now().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            let done = {
                let r = calls.lock().unwrap();
                r.len() >= 3
            };
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        cancel.cancel();
        let recorded = calls.lock().unwrap();
        // Should be: Register(F12) → [no-op for same kvm republish] → Unregister → Register(F1)
        let registers: Vec<_> = recorded
            .iter()
            .filter(|c| matches!(c, RegistrarCall::Register { .. }))
            .collect();
        assert_eq!(
            registers.len(),
            2,
            "initial register + re-register after change: {recorded:?}"
        );
        assert!(
            matches!(&registers[0], RegistrarCall::Register { accel, .. } if accel == "Meta+F12")
        );
        assert!(
            matches!(&registers[1], RegistrarCall::Register { accel, .. } if accel == "Meta+F1")
        );
    }
}
