//! Audio/call inhibitor spawn wrapper.
//!
//! Constructs the production `ReapProbe` and runs the audio poller's tick
//! loop (`audio_source::run_loop`) — both `pub(crate)` seam types, not part
//! of this crate's public doc surface. Mirrors `inhibit_activity.rs:29-39`
//! exactly, including the `None`-returning precedent: spawning nothing when
//! no rule declares an audio-related inhibitor kind.

use dormant_core::config::schema::AudioConfig;
use dormant_core::rules::ControlMsg;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// Re-export AudioRule from audio_source (single source of truth) — mirrors
// `inhibit_activity.rs`'s `pub use crate::idle_source::ActivityRule;`.
pub use crate::audio_source::AudioRule;
use crate::audio_source::{AudioDeps, production_reap_probe, run_loop};

/// Spawn the audio/call inhibitor poller.
///
/// Returns `None` (spawning nothing) when no rule declares `"audio-playback"`
/// or `"call"`.
#[must_use]
pub fn spawn(
    rules: Vec<AudioRule>,
    cfg: AudioConfig,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if rules.is_empty() {
        return None;
    }
    let deps = AudioDeps { ctl, cfg, rules };
    let probe = production_reap_probe();
    Some(tokio::spawn(async move {
        run_loop(deps, probe, cancel).await;
    }))
}
