//! `dormant-tray` binary entry point.
//!
//! Three cfg-gated variants:
//!
//! - **Linux**: spawns the [`ksni`] tray, wires up the IPC loop on a
//!   tokio runtime, and waits for Quit / Ctrl-C.
//! - **macOS**: runs the `AppKit` status item on the process main thread.
//! - **other**: prints an unsupported-platform error and exits 1.
//!   Keeps `cargo check --workspace` green on the Windows/macOS
//!   portability legs (memory-1718 — cross-platform CI gauntlet).

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

#[cfg(target_os = "linux")]
use dormant_core::paths;
#[cfg(target_os = "linux")]
use dormant_tray::DEFAULT_WEB_PORT;
#[cfg(target_os = "linux")]
use dormant_tray::hotkey::HotkeyManager;
#[cfg(target_os = "linux")]
use dormant_tray::hotkey_linux;
#[cfg(target_os = "linux")]
use dormant_tray::ipc_loop;
#[cfg(target_os = "linux")]
use dormant_tray::tray;
#[cfg(target_os = "linux")]
use dormant_tray::tray_state::TrayState;
#[cfg(target_os = "linux")]
use std::sync::Arc;
// `tokio::sync::Mutex` is only used inside `run_linux`; keeping it inside
// the linux-gated block keeps the macOS/Windows stub `main` compiling
// without a `tokio` dependency (memory-1718 — cross-platform CI gauntlet).
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

fn main() -> ExitCode {
    install_tracing();

    #[cfg(target_os = "linux")]
    let result = run_linux();

    #[cfg(target_os = "macos")]
    let result = dormant_tray::tray_macos::run();

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        eprintln!("dormant-tray is not supported on this platform");
        ExitCode::from(1)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dormant-tray: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Initialise tracing-subscriber.  Honours `RUST_LOG`; defaults to `info`
/// for the tray crate only (so the noisy ksni/zbus internals stay quiet).
fn install_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,dormant_tray=info,dormantctl=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Desktop notification helper for the hotkey manager — spawns
/// `notify-send` on the host.
#[cfg(target_os = "linux")]
struct DesktopNotifier;

#[cfg(target_os = "linux")]
impl dormant_tray::hotkey::Notifier for DesktopNotifier {
    fn notify(&self, summary: &str, body: &str) {
        let _ = std::process::Command::new("notify-send")
            .arg(summary)
            .arg(body)
            .arg("--app-name=dormant-tray")
            .arg("--icon=dormant")
            .spawn();
    }
}

#[cfg(target_os = "linux")]
fn run_linux() -> anyhow::Result<()> {
    // Build a tokio runtime — ksni + the IPC loop both expect one.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();

    // Resolve the socket path the same way dormantctl does (the daemon
    // and the CLI agree on this chain — see dormant-core::paths).
    let socket_path = paths::resolve_socket_path(None);

    // Construct shared state, hand an Arc clone to the ksni tray, spawn
    // the tray on the runtime.
    let state = Arc::new(Mutex::new(TrayState::new(socket_path.clone())));
    let tray_handle = handle.block_on(tray::spawn(state.clone(), DEFAULT_WEB_PORT));

    // IPC loop runs on its own task until cancel / Quit.
    let cancel = tokio_util::sync::CancellationToken::new();
    let ipc_cancel = cancel.clone();
    let ipc_state = state.clone();
    let ipc_socket = socket_path.clone();
    let (refresh, refresh_rx) = ipc_loop::refresh_channel();
    let ipc_task = handle.spawn(async move {
        ipc_loop::run(ipc_socket, ipc_state, ipc_cancel, refresh).await;
    });

    // Fan-out the IPC refresh channel so the hotkey manager can
    // subscribe to snapshot publications.  The ksni tray already
    // consumes refresh_rx; we create a watch→broadcast fan-out
    // so both the tray and the hotkey manager can independently
    // react to snapshot changes.
    // Create a new watch channel for the hotkey manager.
    let (hotkey_refresh_tx, hotkey_refresh_rx) = tokio::sync::watch::channel(());
    // Fan-out: forward refresh_rx changes to both the tray (via the
    // ksni handle's internal notification) and the hotkey watch.
    let fanout_cancel = cancel.clone();
    handle.spawn(async move {
        let mut rx = refresh_rx;
        loop {
            tokio::select! {
                () = fanout_cancel.cancelled() => return,
                result = rx.changed() => {
                    if result.is_err() { return; }
                    // Mirror to the hotkey manager's watch channel.
                    let _ = hotkey_refresh_tx.send(());
                }
            }
        }
    });

    // Hotkey manager: watches snapshot publications and registers /
    // unregisters the configured claim hotkey via the XDG Desktop
    // Portal GlobalShortcuts interface.
    let hotkey_cancel = cancel.clone();
    let hotkey_state = state.clone();
    let hotkey_socket = socket_path.clone();
    let hotkey_registrar = hotkey_linux::create_linux_registrar();
    let hotkey_mgr = HotkeyManager::new(
        hotkey_state,
        hotkey_refresh_rx,
        Some(hotkey_registrar),
        Some(Box::new(DesktopNotifier)),
    );
    let capabilities: std::sync::Arc<dyn dormant_tray::dispatch::DispatchCapabilities + 'static> =
        std::sync::Arc::new(dormant_tray::dispatch::SystemCapabilities::new(
            std::sync::Arc::new(tray::request_quit),
        ));
    let hotkey_task = handle.spawn(async move {
        hotkey_mgr
            .run(hotkey_cancel, hotkey_socket, capabilities)
            .await;
    });

    // Wait for either Quit (clicked from the menu) or Ctrl-C.
    let quit_task = handle.spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = tray::wait_for_quit() => {}
        }
    });

    // Block on whichever finishes first.
    handle.block_on(async move {
        quit_task.await.ok();
        cancel.cancel();
        // Give the IPC loop and hotkey manager a moment to drain, then shut down ksni.
        let _ = ipc_task.await;
        let _ = hotkey_task.await;
        tray_handle.shutdown().await;
    });

    Ok(())
}
