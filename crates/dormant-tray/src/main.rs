//! `dormant-tray` binary entry point.
//!
//! Two cfg-gated variants:
//!
//! - **Linux**: spawns the [`ksni`] tray, wires up the IPC loop on a
//!   tokio runtime, and waits for Quit / Ctrl-C.
//! - **other**: prints `"dormant-tray is Linux-only"` and exits 1.
//!   Keeps `cargo check --workspace` green on the Windows/macOS
//!   portability legs (memory-1718 — cross-platform CI gauntlet).

use std::process::ExitCode;
use std::sync::Arc;

use dormant_core::paths;
use tracing_subscriber::EnvFilter;

#[cfg(target_os = "linux")]
use dormant_tray::DEFAULT_WEB_PORT;
#[cfg(target_os = "linux")]
use dormant_tray::ipc_loop;
#[cfg(target_os = "linux")]
use dormant_tray::tray;
#[cfg(target_os = "linux")]
use dormant_tray::tray_state::TrayState;
// `tokio::sync::Mutex` is only used inside `run_linux`; keeping it inside
// the linux-gated block keeps the macOS/Windows stub `main` compiling
// without a `tokio` dependency (memory-1718 — cross-platform CI gauntlet).
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

fn main() -> ExitCode {
    install_tracing();

    #[cfg(target_os = "linux")]
    {
        match run_linux() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("dormant-tray: {e:#}");
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("dormant-tray is Linux-only");
        ExitCode::from(1)
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
    let (refresh, _refresh_rx) = ipc_loop::refresh_channel();
    let ipc_task = handle.spawn(async move {
        ipc_loop::run(ipc_socket, ipc_state, ipc_cancel, refresh).await;
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
        // Give the IPC loop a moment to drain, then shut down ksni.
        let _ = ipc_task.await;
        tray_handle.shutdown().await;
    });

    Ok(())
}
