//! Live smoke test for the Wayland layer-shell backend.
//!
//! Connects to a real compositor (requires `WAYLAND_DISPLAY`), renders the
//! black overlay on the chosen output for `HOLD_SECS`, then tears it down.
//!
//! Run with:
//! ```
//! WAYLAND_DISPLAY=wayland-0 cargo run --example smoke_black -- DP-1
//! ```
//!
//! On the user's `KWin` session this should produce a full-screen black
//! overlay on the named output, returning to the previous contents on exit.

use std::env;
use std::process::ExitCode;
use std::time::Duration;

use dormant_core::traits::RenderSink;
use dormant_core::types::{DisplayId, StageKind};
use dormant_render::LayerShellRenderSink;

const HOLD_SECS: u64 = 3;

fn main() -> ExitCode {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let target = env::args().nth(1).unwrap_or_else(|| "DP-1".into());
    eprintln!("smoke: target output = {target}");
    let sink = match LayerShellRenderSink::new(DisplayId("smoke".into()), target.clone(), None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("smoke: construct sink failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let show = rt.block_on(sink.show(1, 0, StageKind::RenderBlack));
    if let Err(e) = show {
        eprintln!("smoke: show failed: {e}");
        return ExitCode::FAILURE;
    }
    eprintln!("smoke: black overlay up, holding {HOLD_SECS}s");
    std::thread::sleep(Duration::from_secs(HOLD_SECS));
    rt.block_on(sink.teardown(2));
    eprintln!("smoke: teardown OK");
    ExitCode::SUCCESS
}
