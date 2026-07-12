//! Boot-time orchestration: build → (immediate rollback) → single-instance
//! lock → `App::start` → pending-banner re-park (spec §5.1).
//!
//! Deliberately a THIRD module, separate from [`crate::boot_guard`] (pure
//! verdict logic + the sync `prepare` I/O shell) and from [`crate::app`]
//! (the engine itself): `app.rs` is already the largest file in the crate,
//! and boot orchestration — "given a plan, which config actually gets
//! built and started" — is its own concern with a one-way dependency onto
//! `app`/`boot_guard`/`single_instance`, never the reverse. [`boot`] is the
//! ONLY function in the codebase that calls [`App::start`] on a production
//! boot path, and the ONLY one that acquires the single-instance flock —
//! the P1/P15 flock law, upheld by construction (one call site, not a
//! convention to remember).
//!
//! `main.rs` is reduced to: peek options → `boot_guard::prepare` → peek the
//! chosen path's log level → `logging::init` → emit `prepare`'s deferred
//! events → `runtime.block_on(boot(plan, inputs))`.
//!
//! ## `SdNotify` ownership at boot (design note)
//!
//! [`crate::sd_notify::SdNotify`] is deliberately not `Clone` — its own
//! module doc: exactly one owner at a time along the boot chain
//! (`BootInputs::sd_notify` → `App::with_sd_notify` → `Runner`), moved end
//! to end, never fanned out. `App::start` moves it into the `Runner` it
//! constructs partway through its own body, and spec §6.2 wants `READY=1`
//! sent "immediately before the `Ok((handle, join))` return" — by which
//! point the value has already moved into the spawned `run_loop` task and
//! is no longer reachable from `start()`'s own scope.
//!
//! This commit resolves that by reordering `App::start` (see its body) so
//! the `Runner` construction — and therefore the `SdNotify` move — happens
//! LAST, after the IPC listener and the web UI are both spawned, and by
//! calling `self.sd_notify.ready()` on the still-`self`-owned value
//! immediately before it moves into the `Runner` literal. That call is
//! therefore the true final action of `App::start` before its `Ok` return,
//! matching the spec's placement exactly, without giving `SdNotify` a
//! second owner or wrapping it in an `Arc` it would otherwise never need.
//!
//! `boot()` itself never touches `SdNotify` beyond the single
//! `App::with_sd_notify(inputs.sd_notify)` call below — READY delivery is
//! entirely `App::start`'s business; `boot()` only decides WHICH config
//! gets built and started.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;

use dormant_core::rules::ControlMsg;

use crate::app::{App, AppHandle};
use crate::boot_guard::{self, BootInputs, BootPlan, Fingerprint};
use crate::single_instance;

/// The outcome of one [`boot`] call (spec §5.1; T1's literal interface,
/// P12 — `BootPlan` carries no write-back intent, so `boot()` alone decides
/// which of these three shapes the caller sees).
pub enum BootOutcome {
    /// The daemon started. `used_config` is whichever file was actually
    /// built (the plan's chosen config, or the LKG substitute if an
    /// immediate rollback fired); `rolled_back` is true iff it was the LKG.
    Started {
        handle: AppHandle,
        join: JoinHandle<()>,
        used_config: PathBuf,
        rolled_back: bool,
    },
    /// The single-instance flock could not be acquired — another instance
    /// is already running for this user session. `record_discount` has
    /// already been called for `plan.nonce` (spec §5.1 point 5, P2); the
    /// caller (`main.rs`) maps this to `ExitCode::from(1)`.
    LockFailed,
    /// Every build attempt failed — either the plan's chosen config alone
    /// (no LKG, or an LKG that wasn't an eligible fallback), or both the
    /// chosen config AND the LKG substitute. The message is the LAST build
    /// error's display text; the caller maps this to today's
    /// `startup_failed` exit path.
    BuildFailed(String),
}

/// Build → (immediate rollback) → lock → start → re-park pending (spec
/// §5.1). See the module docs for the `SdNotify` ownership note and the
/// P1/P15 flock law this function alone upholds: exactly one
/// `single_instance::acquire` call, immediately before the ONE
/// `App::start()` call site, on every route (the plan's chosen config, an
/// immediately-substituted LKG, or a sticky/counted LKG `prepare` already
/// chose).
///
/// # Errors
///
/// Propagates `App::start` failures (post-build assembly: controller
/// build, post-probe validation, watcher install) verbatim — those are
/// unexpected-at-runtime failures, distinct from the two EXPECTED
/// [`BootOutcome`] failure shapes above.
pub async fn boot(plan: BootPlan, inputs: BootInputs) -> Result<BootOutcome> {
    let lkg_path = boot_guard::lkg_path(&inputs.state_dir);
    // `prepare` already chose the LKG path for `RollBack`/`ContinueRollback`
    // verdicts (spec §5.1 points 3-4) — this is the reliable, cheap way to
    // tell "prepare already decided to substitute" apart from "prepare
    // chose the current config, which may still fail to BUILD" without
    // `BootPlan` needing an extra intent field (P12).
    let is_lkg_chosen = plan.chosen_config == lkg_path;

    let build = App::build(
        plan.chosen_config.clone(),
        inputs.creds_path.clone(),
        inputs.strictness,
    );

    let (app, used_config, rolled_back, immediate_pending) = match build {
        Ok(app) => (app, plan.chosen_config.clone(), is_lkg_chosen, None),
        Err(build_err) => {
            if is_lkg_chosen {
                // Spec §5.1 point 3's "if THAT also fails" branch: the LKG
                // `prepare` already chose could not even be built. No
                // further fallback — today's `startup_failed` behavior.
                return Ok(BootOutcome::BuildFailed(build_err.to_string()));
            }
            match immediate_rollback_eligible(&plan.chosen_config, &lkg_path) {
                Some((current_fp, lkg_fp)) => {
                    let detail = build_err.to_string();
                    // The ONLY boot()-owned crash-loop write (spec §5.1
                    // point 3) — `prepare` could not have predicted this;
                    // it never validated the chosen config, only
                    // fingerprinted it.
                    write_immediate_rollback_state(&inputs.state_dir, current_fp);
                    tracing::error!(
                        event = "config_rollback_boot",
                        failed_fp = ?current_fp,
                        lkg_fp = ?lkg_fp,
                        detail = %detail,
                        "config validation failed at boot; rolling back to last-known-good",
                    );
                    // Concise stderr mirror (cross-family cold-gate,
                    // deepseek fold): a failed config's own `log_level =
                    // "off"` must not hide this from the operator.
                    eprintln!(
                        "dormantd: '{}' failed to start ({detail}); rolled back to last-known-good",
                        plan.chosen_config.display(),
                    );
                    match App::build(
                        lkg_path.clone(),
                        inputs.creds_path.clone(),
                        inputs.strictness,
                    ) {
                        Ok(app) => (
                            app,
                            lkg_path.clone(),
                            true,
                            Some(boot_guard::pending_message_for_rollback(&detail)),
                        ),
                        Err(e2) => return Ok(BootOutcome::BuildFailed(e2.to_string())),
                    }
                }
                None => return Ok(BootOutcome::BuildFailed(build_err.to_string())),
            }
        }
    };

    let app = app.with_sd_notify(inputs.sd_notify);

    // P1+P15: the single flock acquire, immediately before the single
    // `App::start()` call site below, regardless of which of the three
    // branches above produced `app`.
    let lock = match single_instance::acquire(&inputs.lock_path) {
        Ok(guard) => guard,
        Err(e) => {
            // The plan's OWN nonce (P2) — never re-derived from the racy
            // discount-file directory listing.
            boot_guard::record_discount(&inputs.state_dir, plan.nonce);
            tracing::error!(event = "single_instance_lock_failed", error = %e);
            eprintln!("{e}");
            return Ok(BootOutcome::LockFailed);
        }
    };

    let (handle, join) = app.start().await.context("start app")?;

    // `single_instance::SingleInstanceLock` "must be held for the daemon's
    // entire lifetime" (its own module doc) — but `boot()` returns here,
    // long before the daemon actually stops (`join` is still pending, and
    // `BootOutcome::Started`'s fields are pinned literally, T1/P12 — no
    // lock field to carry it out through). `mem::forget` intentionally
    // skips `lock`'s `Drop` (closing the fd), so the flock stays held for
    // the rest of the OS process's life — exactly the semantics the module
    // doc already promises ("the kernel also releases the flock on process
    // death, so crash-safe"): this makes that release point literally true
    // (process exit) instead of merely aspirational.
    #[allow(
        clippy::mem_forget,
        reason = "documented above: intentional process-lifetime leak"
    )]
    std::mem::forget(lock);

    // Re-park the pending-reload banner (spec §5.1 point 6, F9/F11): the
    // immediate-rollback branch above builds its own message (since
    // `prepare` never saw this failure); `prepare` already built one for
    // counted `RollBack`, sticky `ContinueRollback`, and quiet-retry
    // `Proceed` paths. At most one of the two is `Some`.
    if let Some(msg) = immediate_pending.or(plan.pending_message) {
        let _ = handle
            .control_sender()
            .send(ControlMsg::SetPendingReload(Some(msg)))
            .await;
    }

    Ok(BootOutcome::Started {
        handle,
        join,
        used_config,
        rolled_back,
    })
}

/// Spec §5.1 point 3's immediate-rollback ELIGIBILITY check: an LKG file
/// exists AND its bytes differ from `chosen`'s (direct comparison, F7; an
/// unreadable `chosen` is treated as differing, F14 — `files_bytes_equal`
/// already returns `false` whenever either read fails, so no separate
/// unreadable-handling branch is needed here). Returns the two fingerprints
/// for the `config_rollback_boot` log event when eligible.
fn immediate_rollback_eligible(
    chosen: &Path,
    lkg_path: &Path,
) -> Option<(Fingerprint, Fingerprint)> {
    if !lkg_path.exists() {
        return None;
    }
    if boot_guard::files_bytes_equal(lkg_path, chosen) {
        return None;
    }
    Some((
        boot_guard::fingerprint_file(chosen),
        boot_guard::fingerprint_file(lkg_path),
    ))
}

/// Write `rollback_active: true` + `rolled_back_from: failed_fp` directly
/// into `crash-loop.json` — the ONLY `boot()`-owned crash-loop write (spec
/// §5.1 point 3; every OTHER verdict-driven write lives in
/// `boot_guard::prepare`, P12). Best-effort, matching `prepare`'s own
/// swallow-on-failure posture: a corrupt write must never block boot.
fn write_immediate_rollback_state(state_dir: &Path, failed_fp: Fingerprint) {
    let mut state = boot_guard::load_crash_loop_state(state_dir);
    state.rollback_active = true;
    state.rolled_back_from = Some(failed_fp);
    let _ = boot_guard::write_atomic_json(state_dir, boot_guard::CRASH_LOOP_FILE, &state);
}
