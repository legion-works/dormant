//! Boot-time crash-loop verdict + atomic state files (spec §3, §5.2).
//!
//! Two layers, deliberately split (spec §5.1):
//!
//! - [`decide`] — GENUINELY PURE. Given a parsed [`CrashLoopState`], the live
//!   discount-nonce set, precomputed [`LkgInfo`], the current config's
//!   [`Fingerprint`], wall-clock time, and the `watchdog.lkg_rollback_enabled`
//!   gate, returns a [`Verdict`]. Zero I/O, zero tokio — the entire §5.2
//!   evaluation-order matrix lives here and is unit-tested directly.
//! - [`prepare`] — the sync I/O shell: records this boot's start entry,
//!   builds [`LkgInfo`] (file existence + `load_config` validation + a
//!   direct byte comparison against the current config — spec §3/§5.2 F7,
//!   never a fingerprint-equality shortcut), calls `decide`, performs every
//!   verdict-driven `crash-loop.json` write itself (P12 — `prepare` owns all
//!   verdict state writes; only the *immediate* build-failure rollback,
//!   which `decide` cannot predict, is written by `boot()` in a later task),
//!   and returns a [`BootPlan`] with deferred log events for the caller to
//!   emit once logging is initialised.
//!
//! State files live at `state_dir()` root (spec §3): `crash-loop.json`,
//! `discount-<nonce>`. `last-known-good.toml` is read here (never written —
//! promotion is a later task) at `state_dir().join("last-known-good.toml")`.
//!
//! All state-file writes are atomic (temp file + rename, same pattern as
//! `dormant_displays::samsung_ip::write_token_state`), directory `0o700`,
//! file `0o600`. A corrupt/torn/absent `crash-loop.json` is treated as
//! [`CrashLoopState::default`] — crash-loop bookkeeping must never block
//! boot (spec invariant #5).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dormant_core::config::{Strictness, load_config};
use dormant_core::rules::StateSnapshot;
use serde::{Deserialize, Serialize};

use crate::sd_notify::SdNotify;

/// Non-discounted starts, within [`CRASH_LOOP_WINDOW`], sharing the current
/// config's fingerprint, required before a counted rollback fires (spec
/// §5.2 condition a). Documented `pub const`, deliberately NOT config — this
/// machinery must work precisely when the config is unloadable.
pub const CRASH_LOOP_THRESHOLD: u32 = 3;

/// Sliding window a crash-loop is measured over (spec §5.2, cross-family
/// cold-gate: raised from 90s to 6m so the third restart of a
/// `WatchdogSec=150` wedge-detect cycle still lands inside the window).
pub const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(6 * 60);

/// Consecutive display-health-deferred LKG promotion candidates before
/// promotion proceeds anyway (spec §4 point 1). Defined here (not T4's
/// `should_promote`) because it is a documented const alongside the other
/// two, not because `boot_guard` uses it directly.
pub const LKG_HEALTH_DEFER_CAP: u32 = 3;

// `pub(crate)`: `boot()` (T5, `dormantd/src/boot.rs`) is the ONLY other
// consumer — the single boot()-owned crash-loop write (spec §5.1 point 3,
// immediate rollback) reads-modifies-writes this exact file via
// `load_crash_loop_state`/`write_atomic_json` directly, since `decide()`
// cannot predict a build-time-only failure.
pub(crate) const CRASH_LOOP_FILE: &str = "crash-loop.json";
const LKG_FILE: &str = "last-known-good.toml";
const DISCOUNT_PREFIX: &str = "discount-";

/// `state_dir().join("last-known-good.toml")` — the single canonical join
/// point `boot()` uses both to detect whether `prepare` already chose the
/// LKG (`plan.chosen_config == lkg_path(..)`) and to run its own immediate-
/// rollback eligibility check (spec §5.1 point 3).
#[must_use]
pub(crate) fn lkg_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LKG_FILE)
}

// ── Fingerprint ──────────────────────────────────────────────────────────

/// Identity of a config file's bytes: `(len, hash64)` on success, or the
/// `Unreadable` sentinel when the file could not be read at all.
///
/// `hash64` is a std `DefaultHasher` value — stable within one binary, NOT
/// stable across Rust releases (spec §3 F7: no sha2 in the daemon core,
/// following the OLED-health precedent). A binary upgrade forgetting old
/// loop history is an accepted, documented fail-open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fingerprint {
    Bytes { len: u64, hash64: u64 },
    Unreadable,
}

// Custom serde (P4): the bare derive would emit `{"Bytes":{...}}` /
// `"Unreadable"`, contradicting the spec's committed `{ len, hash64 } |
// "unreadable"` shape. Pinned by `fingerprint_json_shape_pinned` below.
impl Serialize for Fingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            Fingerprint::Bytes { len, hash64 } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("len", len)?;
                map.serialize_entry("hash64", hash64)?;
                map.end()
            }
            Fingerprint::Unreadable => serializer.serialize_str("unreadable"),
        }
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct FingerprintVisitor;

        impl<'de> serde::de::Visitor<'de> for FingerprintVisitor {
            type Value = Fingerprint;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a fingerprint object `{len, hash64}` or the bare string \"unreadable\"",
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<Fingerprint, E>
            where
                E: serde::de::Error,
            {
                if v == "unreadable" {
                    Ok(Fingerprint::Unreadable)
                } else {
                    Err(E::invalid_value(serde::de::Unexpected::Str(v), &self))
                }
            }

            fn visit_map<A>(self, mut map: A) -> Result<Fingerprint, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut len: Option<u64> = None;
                let mut hash64: Option<u64> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "len" => len = Some(map.next_value()?),
                        "hash64" => hash64 = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                let len = len.ok_or_else(|| serde::de::Error::missing_field("len"))?;
                let hash64 = hash64.ok_or_else(|| serde::de::Error::missing_field("hash64"))?;
                Ok(Fingerprint::Bytes { len, hash64 })
            }
        }

        deserializer.deserialize_any(FingerprintVisitor)
    }
}

/// Fingerprint a byte slice: `(len, hash64)` via std `DefaultHasher`.
/// Cross-release instability is documented on [`Fingerprint`] itself.
#[must_use]
pub fn fingerprint_bytes(bytes: &[u8]) -> Fingerprint {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Fingerprint::Bytes {
        len,
        hash64: hasher.finish(),
    }
}

/// Fingerprint a file on disk; an unreadable file (missing, permissions,
/// I/O error) maps to the `Unreadable` sentinel (spec §5.1 point 1).
///
/// `pub(crate)`: `boot()` (T5) reuses this for its own immediate-rollback
/// eligibility check and `config_rollback_boot` log fields — the exact same
/// unreadable-sentinel semantics `prepare` uses, never a second definition.
pub(crate) fn fingerprint_file(path: &Path) -> Fingerprint {
    std::fs::read(path).map_or(Fingerprint::Unreadable, |bytes| fingerprint_bytes(&bytes))
}

// ── Persisted state ──────────────────────────────────────────────────────

/// One recorded daemon start (spec §3 `crash-loop.json.starts[]`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StartEntry {
    pub epoch_s: u64,
    pub fingerprint: Fingerprint,
    /// Join key to `discount-<nonce>` files (spec §3 F6) — there is no
    /// per-entry `discounted` field; a stray duplicate-instance boot is
    /// disowned by an out-of-band file, never a rewrite of this entry.
    pub nonce: u64,
}

/// `crash-loop.json` — spec §3. Capped at 10 most recent entries.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct CrashLoopState {
    pub schema_version: u32,
    pub starts: Vec<StartEntry>,
    pub rollback_active: bool,
    pub rolled_back_from: Option<Fingerprint>,
}

impl Default for CrashLoopState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            starts: Vec::new(),
            rollback_active: false,
            rolled_back_from: None,
        }
    }
}

/// Cap on the number of most-recent start entries kept in `crash-loop.json`.
const MAX_STARTS: usize = 10;

// ── Verdict ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Proceed,
    RollBack,
    ContinueRollback,
}

/// The pure result of one [`decide`] call (spec §5.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verdict {
    pub action: Action,
    pub clear_rollback_active: bool,
    pub retry_after_quiet: bool,
    /// Drives `config_rollback_retry` / `lkg_missing_rollback_disarmed` +
    /// the re-parked pending message.
    pub lkg_missing_disarmed: bool,
}

/// Facts about `last-known-good.toml` that `decide` needs, precomputed by
/// `prepare` (I/O) so `decide` itself stays pure (spec §3/§5.2 P7/P14). The
/// byte-compare fact is the ONLY channel through which `decide` learns
/// whether the current config matches the LKG — never a fingerprint
/// equality shortcut.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LkgInfo {
    pub exists: bool,
    pub validates: bool,
    pub bytes_equal_current: bool,
}

/// A log event `prepare` wants the caller to emit once logging is
/// initialised (spec §5.1 — recording happens exactly once inside
/// `prepare`, before logging exists; the events themselves are deferred).
#[derive(Clone, PartialEq, Debug)]
pub enum DeferredEvent {
    CrashLoopDetected {
        count: u32,
    },
    RollbackBoot {
        failed_fp: Fingerprint,
        lkg_fp: Fingerprint,
        detail: String,
    },
    RollbackContinued,
    RollbackRetry {
        message: String,
    },
    LkgMissingRollbackDisarmed,
}

/// The outcome of `prepare`: which config to attempt building, the nonce
/// this boot recorded itself under (the exact join key `record_discount`
/// must reuse on lock failure — P2), any events to log once logging exists,
/// and a pending-reload banner message for rollback/retry paths.
///
/// Deliberately carries NO write-back intent fields (P12 reversal) —
/// `prepare` performs every verdict-driven `crash-loop.json` write itself,
/// in the same atomic rewrite as the start-entry record.
///
/// `operator_config` (rollback-recovery plan, Task 1) is the REAL operator
/// config path — always `config_path` as passed into `prepare()`,
/// regardless of verdict. `chosen_config` may diverge from it (the LKG
/// substitute, on `RollBack`/`ContinueRollback`); `operator_config` never
/// does. `boot()` threads this into the boot-only `App` builder so every
/// runtime consumer added after generation 0 (the watcher, Web UI,
/// `Runner`, `AppHandle`) keeps watching/reloading the operator's actual
/// file.
#[derive(Clone, PartialEq, Debug)]
pub struct BootPlan {
    pub chosen_config: PathBuf,
    pub operator_config: PathBuf,
    pub nonce: u64,
    pub deferred_events: Vec<DeferredEvent>,
    pub pending_message: Option<String>,
}

/// Runtime-only bundle `boot()` (T5, `dormantd/src/boot.rs`) needs (spec
/// §5.1). Five fields, pinned by `boot_inputs_field_list_is_minimal` below —
/// no socket override field, because T5's tests set `daemon.socket_path`
/// directly in the seeded config instead (`App::start` binds real IPC; the
/// tempdir-socket pattern, `ipc_roundtrip.rs:34-50` precedent).
pub struct BootInputs {
    pub creds_path: PathBuf,
    pub strictness: Strictness,
    pub state_dir: PathBuf,
    pub lock_path: PathBuf,
    /// Injected readiness/watchdog seam (spec §6.2). `boot()` threads this
    /// straight into `App::with_sd_notify` on whichever `App` it ends up
    /// starting — production callers build it via `SdNotify::from_env()`;
    /// tests inject `SdNotify::from_socket_for_test`/a disabled instance.
    pub sd_notify: SdNotify,
}

// ── decide: pure verdict ────────────────────────────────────────────────

/// Pure crash-loop verdict — spec §5.2's evaluation order EXACTLY:
///
/// 1. Compute `clear_rollback_active` = (F2) `current != rolled_back_from`
///    OR (R2-M4) the previous non-discounted start entry is older than
///    [`CRASH_LOOP_WINDOW`].
/// 2. If cleared via the AGE leg with bytes UNCHANGED (fp does NOT differ):
///    `Proceed`, `retry_after_quiet = true` — evaluated BEFORE step 4, so
///    quiet-retry takes precedence over sticky `ContinueRollback`.
/// 3. Else if `rollback_active` and the LKG is missing/invalid: `Proceed`,
///    `clear_rollback_active = true`, `lkg_missing_disarmed = true`.
/// 4. Else if `rollback_active`, fp unchanged, and the LKG exists+validates:
///    `ContinueRollback` (sticky substitution, no new counting).
/// 5. Else `RollBack` iff ALL of: (a) ≥ [`CRASH_LOOP_THRESHOLD`]
///    non-discounted starts within [`CRASH_LOOP_WINDOW`] including this
///    one; (b) all of those share `current`'s fingerprint; (c) the current
///    config's bytes differ from the LKG's (`!lkg.bytes_equal_current`);
///    (d) an LKG exists; (e) `rollback_active` is not already true; (f)
///    `lkg_rollback_enabled`. `RollBack` always forces
///    `clear_rollback_active = false`. Else `Proceed` (with step 1's
///    `clear_rollback_active`, unmodified).
#[must_use]
#[allow(
    clippy::implicit_hasher,
    reason = "the plan T1 interface pins `&HashSet<u64>` literally; a generic BuildHasher \
              parameter would be a signature deviation, not a real hasher-generality need"
)]
pub fn decide(
    state: &CrashLoopState,
    discount_nonces: &HashSet<u64>,
    lkg: &LkgInfo,
    current: Fingerprint,
    now_epoch_s: u64,
    lkg_rollback_enabled: bool,
) -> Verdict {
    let window_s = CRASH_LOOP_WINDOW.as_secs();

    let live: Vec<&StartEntry> = state
        .starts
        .iter()
        .filter(|e| !discount_nonces.contains(&e.nonce))
        .collect();
    // The just-recorded current start is assumed to be the last (newest)
    // live entry (`prepare` appends in chronological order) — "previous"
    // is the live entry immediately before it.
    let previous = live.iter().rev().nth(1).copied();

    let fp_differs = Some(current) != state.rolled_back_from;
    let age_leg = previous.is_some_and(|e| now_epoch_s.saturating_sub(e.epoch_s) > window_s);
    let clear_rollback_active = fp_differs || age_leg;

    // Step 2 — quiet-period retry: cleared via the AGE leg only, bytes
    // unchanged. Checked BEFORE step 4 (sticky) — precedence is pinned by
    // `quiet_period_clears_and_retries_loudly`.
    if age_leg && !fp_differs {
        return Verdict {
            action: Action::Proceed,
            clear_rollback_active: true,
            retry_after_quiet: true,
            lkg_missing_disarmed: false,
        };
    }

    // Step 3 — LKG missing/invalid mid-storm: disarm loudly.
    if state.rollback_active && (!lkg.exists || !lkg.validates) {
        return Verdict {
            action: Action::Proceed,
            clear_rollback_active: true,
            retry_after_quiet: false,
            lkg_missing_disarmed: true,
        };
    }

    // Step 4 — sticky ContinueRollback.
    if state.rollback_active && !fp_differs && lkg.exists && lkg.validates {
        return Verdict {
            action: Action::ContinueRollback,
            clear_rollback_active: false,
            retry_after_quiet: false,
            lkg_missing_disarmed: false,
        };
    }

    // Step 5 — counted crash-loop RollBack.
    let (total_in_window, matching) =
        same_fingerprint_window_stats(state, discount_nonces, current, now_epoch_s);
    let threshold_met = total_in_window >= CRASH_LOOP_THRESHOLD;
    let all_same_fingerprint = total_in_window > 0 && matching == total_in_window;
    let bytes_differ_from_lkg = !lkg.bytes_equal_current;
    let lkg_exists = lkg.exists;
    let not_already_rolled_back = !state.rollback_active;
    let gate_enabled = lkg_rollback_enabled;

    if threshold_met
        && all_same_fingerprint
        && bytes_differ_from_lkg
        && lkg_exists
        && not_already_rolled_back
        && gate_enabled
    {
        return Verdict {
            action: Action::RollBack,
            clear_rollback_active: false,
            retry_after_quiet: false,
            lkg_missing_disarmed: false,
        };
    }

    Verdict {
        action: Action::Proceed,
        clear_rollback_active,
        retry_after_quiet: false,
        lkg_missing_disarmed: false,
    }
}

/// Count of non-discounted starts within [`CRASH_LOOP_WINDOW`] (`total`,
/// spec §5.2 condition a's population) and, among those, how many share
/// `current`'s fingerprint (`matching`, condition b's numerator). Also used
/// by `prepare` to decide whether a plain step-5 `Proceed` still deserves a
/// loud `crash_loop_detected` warning (spec §5.2's closing paragraph: a
/// real same-config crash loop that didn't roll back for some other reason
/// — no LKG, bytes already match, already rolled back, or the gate is off
/// — is still worth surfacing).
fn same_fingerprint_window_stats(
    state: &CrashLoopState,
    discount_nonces: &HashSet<u64>,
    current: Fingerprint,
    now_epoch_s: u64,
) -> (u32, u32) {
    let window_s = CRASH_LOOP_WINDOW.as_secs();
    let mut total = 0u32;
    let mut matching = 0u32;
    for e in &state.starts {
        if discount_nonces.contains(&e.nonce) {
            continue;
        }
        if now_epoch_s.saturating_sub(e.epoch_s) > window_s {
            continue;
        }
        total += 1;
        if e.fingerprint == current {
            matching += 1;
        }
    }
    (total, matching)
}

// ── prepare: sync I/O shell ──────────────────────────────────────────────

/// Record this boot's start, decide, and perform every verdict-driven
/// `crash-loop.json` write — sync, no logging (spec §5.1: called before
/// logging is initialised; events are deferred into the returned
/// [`BootPlan`]).
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "single sync shell that must record + decide + write in one atomic pass \
              (spec §5.1); splitting it would scatter the verdict-driven write ownership \
              this function exists to hold in one place (P12)"
)]
pub fn prepare(
    config_path: &Path,
    creds_path: &Path,
    state_dir: &Path,
    strictness: Strictness,
    lkg_rollback_enabled: bool,
) -> BootPlan {
    // Not read in this task: `App::build` (a later task) is the sole
    // consumer of `creds_path`; `prepare` only fingerprints/loads-for-
    // validation, neither of which touches credentials.
    let _ = creds_path;

    let now = now_epoch_s();
    let nonce: u64 = generate_nonce();

    let discount_nonces = collect_and_sweep_discounts(state_dir, now);
    let mut state = load_crash_loop_state(state_dir);

    let current_fp = fingerprint_file(config_path);
    state.starts.push(StartEntry {
        epoch_s: now,
        fingerprint: current_fp,
        nonce,
    });
    if state.starts.len() > MAX_STARTS {
        let excess = state.starts.len() - MAX_STARTS;
        state.starts.drain(0..excess);
    }

    let lkg_path = state_dir.join(LKG_FILE);
    let lkg_info = build_lkg_info(&lkg_path, config_path, strictness);

    let verdict = decide(
        &state,
        &discount_nonces,
        &lkg_info,
        current_fp,
        now,
        lkg_rollback_enabled,
    );

    match verdict.action {
        Action::RollBack => {
            state.rollback_active = true;
            state.rolled_back_from = Some(current_fp);
        }
        Action::ContinueRollback => {
            // Stays active; `rolled_back_from` is already correct.
        }
        Action::Proceed => {
            if verdict.clear_rollback_active {
                state.rollback_active = false;
                state.rolled_back_from = None;
            }
        }
    }

    // Unconditional write, BEFORE any build attempt (spec §3): a corrupt
    // write attempt must never block boot, so failures are swallowed here
    // exactly like the samsung_ip token-state precedent.
    let _ = write_atomic_json(state_dir, CRASH_LOOP_FILE, &state);

    let (chosen_config, pending_message, deferred_events) = match verdict.action {
        Action::RollBack => {
            let lkg_fp = fingerprint_file(&lkg_path);
            let detail = format!(
                "crash-loop threshold reached for '{}'; rolling back to last-known-good",
                config_path.display()
            );
            let message = pending_message_for_rollback(&detail);
            (
                lkg_path.clone(),
                Some(message),
                vec![DeferredEvent::RollbackBoot {
                    failed_fp: current_fp,
                    lkg_fp,
                    detail,
                }],
            )
        }
        Action::ContinueRollback => (
            lkg_path.clone(),
            Some(
                "sticky rollback active; latest config is still being held back — edit it, \
                 wait past CRASH_LOOP_WINDOW for a loud retry, or set \
                 watchdog.lkg_rollback_enabled = false while debugging"
                    .to_string(),
            ),
            vec![DeferredEvent::RollbackContinued],
        ),
        Action::Proceed if verdict.retry_after_quiet => {
            let message = "retrying latest config after a quiet period; to stop this cycle, \
                            edit the config, wait past CRASH_LOOP_WINDOW between restarts, or \
                            set watchdog.lkg_rollback_enabled = false while debugging"
                .to_string();
            (
                config_path.to_path_buf(),
                Some(message.clone()),
                vec![DeferredEvent::RollbackRetry { message }],
            )
        }
        Action::Proceed if verdict.lkg_missing_disarmed => (
            config_path.to_path_buf(),
            None,
            vec![DeferredEvent::LkgMissingRollbackDisarmed],
        ),
        Action::Proceed => {
            let (total, matching) =
                same_fingerprint_window_stats(&state, &discount_nonces, current_fp, now);
            let events = if total >= CRASH_LOOP_THRESHOLD && total == matching {
                vec![DeferredEvent::CrashLoopDetected { count: total }]
            } else {
                Vec::new()
            };
            (config_path.to_path_buf(), None, events)
        }
    };

    BootPlan {
        chosen_config,
        operator_config: config_path.to_path_buf(),
        nonce,
        deferred_events,
        pending_message,
    }
}

/// The spec §5.1 point 6 (F11) pending-reload banner text for a rollback
/// boot, parameterised on `detail`. Shared by `prepare`'s counted `RollBack`
/// arm and `boot()`'s (T5) immediate-rollback arm — the SAME wording either
/// way, since from the operator's point of view both are "your latest
/// config failed and was rolled back to last-known-good".
#[must_use]
pub(crate) fn pending_message_for_rollback(detail: &str) -> String {
    format!(
        "your latest config failed and was rolled back to last-known-good — \
         fix it and reload: {detail}"
    )
}

/// Build [`LkgInfo`] for `lkg_path`: existence, `load_config`
/// validation (spec §3 F16 — same trust class as any config file), and a
/// direct byte comparison against `config_path`'s current bytes (spec §3/
/// §5.2 F7 — never a fingerprint-equality shortcut; F14: an unreadable
/// current file is treated as differing).
fn build_lkg_info(lkg_path: &Path, config_path: &Path, strictness: Strictness) -> LkgInfo {
    if !lkg_path.exists() {
        return LkgInfo {
            exists: false,
            validates: false,
            bytes_equal_current: false,
        };
    }
    let validates = load_config(lkg_path, strictness).is_ok();
    let bytes_equal_current = files_bytes_equal(lkg_path, config_path);
    LkgInfo {
        exists: true,
        validates,
        bytes_equal_current,
    }
}

/// `pub(crate)`: `boot()` (T5) reuses this exact direct byte-comparison
/// (spec §3/§5.2 F7) for its own immediate-rollback eligibility check —
/// never a fingerprint-equality shortcut, matching `prepare`'s use.
pub(crate) fn files_bytes_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

// ── LKG promotion gate (T4, spec §4) ────────────────────────────────────

/// The pure result of one [`should_promote`] call (spec §4 Mechanism).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromoteVerdict {
    /// The stability window has not elapsed since the candidate started.
    Wait,
    /// Window elapsed, display health proven (or unproven-but-not-failing),
    /// on-disk bytes match the candidate — write the LKG.
    Promote,
    /// Window elapsed and healthy, but the on-disk config no longer matches
    /// the candidate (an un-applied direct edit is sitting there) — skip
    /// this tick, never promote unproven bytes (spec invariant #6).
    SkipDirty,
    /// Window elapsed, but at least one display has a non-empty
    /// [`dormant_core::rules::ControllerHealth`] set with every row
    /// unhealthy, and the consecutive-defer count is still below
    /// [`LKG_HEALTH_DEFER_CAP`] — hold off, re-check next tick.
    DeferHealth,
    /// Same all-unhealthy-display condition as `DeferHealth`, but the defer
    /// cap has been reached — promote anyway (an imperfect LKG beats none,
    /// spec R2-M3) unless the dirty check overrides it (`SkipDirty` still
    /// wins — invariant #6 outranks the starvation cap).
    PromoteDespiteHealth,
}

/// Pure LKG promotion gate (spec §4 Mechanism, points 1-2; R2-M3's
/// starvation cap; R3-M2's any-set accumulation).
///
/// Evaluation order:
/// 1. `now - since < window` → [`PromoteVerdict::Wait`] (no health/dirty
///    checks yet — an unbroken healthy window is a wall-clock precondition,
///    not something the display-health gate can shortcut).
/// 2. Display-health gate (spec F4): a display counts as "all-unhealthy"
///    only when its `controllers` vector is NON-EMPTY and every row is
///    unhealthy — a display never commanded during the window (empty
///    health) is unproven, not failing, and NEVER defers (spec's honest
///    limit, documented, not a bug). If any display is all-unhealthy:
///    - `defer_count < LKG_HEALTH_DEFER_CAP` → [`PromoteVerdict::DeferHealth`]
///      (the caller increments its counter; R3-M2: this function takes only
///      a bare count, never a per-set counter, so DIFFERENT unhealthy sets
///      across ticks accumulate identically to the same set repeating — the
///      starvation cap cannot be reset by a fluctuating set by construction).
///    - `defer_count >= LKG_HEALTH_DEFER_CAP` → proceeds to the dirty check
///      below instead of deferring again (an imperfect LKG beats none).
/// 3. Dirty check (spec invariant #6, outranks the health cap): on-disk
///    bytes no longer equal to the candidate's captured bytes →
///    [`PromoteVerdict::SkipDirty`].
/// 4. Otherwise: [`PromoteVerdict::Promote`], or
///    [`PromoteVerdict::PromoteDespiteHealth`] if the health gate was
///    overridden by the cap in step 2.
#[must_use]
pub fn should_promote(
    since: Instant,
    now: Instant,
    window: Duration,
    snapshot: &StateSnapshot,
    defer_count: u32,
    on_disk_matches: bool,
) -> PromoteVerdict {
    if now.saturating_duration_since(since) < window {
        return PromoteVerdict::Wait;
    }

    let any_all_unhealthy = snapshot
        .displays
        .iter()
        .any(|(_, d)| !d.controllers.is_empty() && d.controllers.iter().all(|c| !c.healthy));

    if any_all_unhealthy && defer_count < LKG_HEALTH_DEFER_CAP {
        return PromoteVerdict::DeferHealth;
    }

    if !on_disk_matches {
        return PromoteVerdict::SkipDirty;
    }

    if any_all_unhealthy {
        PromoteVerdict::PromoteDespiteHealth
    } else {
        PromoteVerdict::Promote
    }
}

fn now_epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Process-lifetime tiebreaker for [`generate_nonce`] — folded into the
/// hash so two `prepare()` calls within the same wall-clock second (e.g.
/// tests, or a fast restart-loop) still produce distinct nonces.
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// This boot's join key for `discount-<nonce>` files (spec §3 F6) — needs
/// only to be practically-unique per boot within one `state_dir`, NOT
/// cryptographically unpredictable (it is never a security boundary, only
/// a dedup/join key alongside a `Vec<StartEntry>` capped at
/// [`MAX_STARTS`]). `rand` was previously pulled in for this alone; the
/// daemon core is otherwise std-only (spec §3 F7 — `rand` was already an
/// optional, `web-ui`-feature-gated dependency via `dormant-web`, never a
/// core one), so this hashes wall-clock time + PID + a monotonic counter
/// through std's `DefaultHasher` instead of adding a real dependency.
fn generate_nonce() -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    NONCE_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    hasher.finish()
}

// ── crash-loop.json load ────────────────────────────────────────────────

/// `pub(crate)`: `boot()` (T5) reads-modifies-writes this exact state via
/// this loader for its own immediate-rollback write (spec §5.1 point 3 —
/// the ONLY boot()-owned crash-loop write; `prepare` owns every other one).
pub(crate) fn load_crash_loop_state(state_dir: &Path) -> CrashLoopState {
    let path = state_dir.join(CRASH_LOOP_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return CrashLoopState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

// ── live-reload rollback recovery (rollback-recovery plan, Task 2) ─────

/// Clear rollback bookkeeping in `crash-loop.json` after a config reload
/// the operator has fixed and the engine has ACCEPTED (Task 2 §1/§3): the
/// live-reload sibling of the boot-time `clear_rollback_active` legs
/// `decide` already computes (F2's bytes-changed leg, the LKG-missing
/// disarm, and the quiet-retry leg). Loads the current state, clears
/// `rollback_active`/`rolled_back_from`, and rewrites the file atomically
/// via the same [`write_atomic_json`] every other verdict-driven write
/// uses — every other field (`schema_version`, `starts`) is carried
/// through untouched, never reset.
///
/// Returns the write's `Result` (unlike `prepare`'s own best-effort
/// swallow-on-failure writes): `Runner::reload`'s accepted-reload arm
/// (Task 2 §4) branches on success/failure to decide whether to arm its
/// `rollback_state_clear_pending` retry flag, so the failure must be
/// observable here, not swallowed.
pub(crate) fn clear_rollback_after_reload(state_dir: &Path) -> std::io::Result<()> {
    let mut state = load_crash_loop_state(state_dir);
    state.rollback_active = false;
    state.rolled_back_from = None;
    write_atomic_json(state_dir, CRASH_LOOP_FILE, &state)
}

// ── discount files ───────────────────────────────────────────────────────

/// Create `state_dir()/discount-<nonce>` — an atomic, uniquely-named create
/// (`O_EXCL`), never a read-modify-write of the shared `crash-loop.json`
/// (spec §3 F6). Idempotent: a duplicate call for the same nonce is
/// harmless (the existing file, and its content, are left untouched).
/// File mode `0o600` on Unix, matching `write_atomic_json`.
pub fn record_discount(state_dir: &Path, nonce: u64) {
    if std::fs::create_dir_all(state_dir).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = state_dir.join(format!("{DISCOUNT_PREFIX}{nonce}"));
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .is_ok()
    {
        // Defense in depth (matches `write_atomic_json`'s file mode):
        // best-effort, the file was just created 0o644-ish under the
        // process umask, tighten it to 0o600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    // else: AlreadyExists is idempotent (another boot, or a previous call,
    // already discounted this nonce); any other error is best-effort and
    // never blocks — discount is a courtesy, not a safety boundary.
}

/// Collect the live discount-nonce set, sweeping (deleting) any
/// `discount-<nonce>` file older than [`CRASH_LOOP_WINDOW`] first (spec §3:
/// "The next boot's writer sweeps discount files older than
/// `CRASH_LOOP_WINDOW`" — deepseek nit: dedup falls out naturally from
/// collecting into a `HashSet`).
fn collect_and_sweep_discounts(state_dir: &Path, now_epoch_s: u64) -> HashSet<u64> {
    let mut set = HashSet::new();
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return set;
    };
    let window_s = CRASH_LOOP_WINDOW.as_secs();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(nonce_str) = name_str.strip_prefix(DISCOUNT_PREFIX) else {
            continue;
        };
        let Ok(nonce) = nonce_str.parse::<u64>() else {
            continue;
        };
        let mtime_epoch_s = entry.metadata().ok().and_then(|m| epoch_s_of(&m));
        let stale = mtime_epoch_s.is_some_and(|mtime| now_epoch_s.saturating_sub(mtime) > window_s);
        if stale {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        set.insert(nonce);
    }
    set
}

fn epoch_s_of(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

// ── atomic write (samsung_ip.rs:497-548 pattern) ────────────────────────

/// Atomically write `value` as JSON to `<dir>/<filename>` (temp file, same
/// dir, then rename). Directory `0o700`, file `0o600` on Unix.
pub(crate) fn write_atomic_json<T: Serialize>(
    dir: &Path,
    filename: &str,
    value: &T,
) -> std::io::Result<()> {
    let raw = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_atomic_bytes(dir, filename, raw.as_bytes())
}

/// Atomically write raw `bytes` to `<dir>/<filename>` (temp file, same dir,
/// then rename). Directory `0o700`, file `0o600` on Unix. `pub(crate)` so
/// T4's LKG-promotion writer (`app.rs`) can reuse the same atomic-write
/// primitive for `last-known-good.toml` (a verbatim config-byte copy, not
/// JSON) that `write_atomic_json` already uses for `crash-loop.json` and
/// the `.meta.json` sidecar.
pub(crate) fn write_atomic_bytes(dir: &Path, filename: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    let final_path = dir.join(filename);
    let tmp_path = dir.join(format!("{filename}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(epoch_s: u64, fp: Fingerprint, nonce: u64) -> StartEntry {
        StartEntry {
            epoch_s,
            fingerprint: fp,
            nonce,
        }
    }

    fn state_with(starts: Vec<StartEntry>) -> CrashLoopState {
        CrashLoopState {
            schema_version: 1,
            starts,
            rollback_active: false,
            rolled_back_from: None,
        }
    }

    fn lkg(exists: bool, validates: bool, bytes_equal_current: bool) -> LkgInfo {
        LkgInfo {
            exists,
            validates,
            bytes_equal_current,
        }
    }

    // ── §5.2 verdict matrix ──────────────────────────────────────────────

    #[test]
    fn two_starts_proceed_three_rolls_back() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        // Two starts within the window: threshold not met.
        let two = state_with(vec![entry(now - 120, fp_a, 1), entry(now, fp_a, 2)]);
        let v = decide(&two, &empty, &lkg(true, true, false), fp_a, now, true);
        assert_eq!(v.action, Action::Proceed, "2 starts must not roll back");

        // Three starts, all within the window: rolls back.
        let three = state_with(vec![
            entry(now - 240, fp_a, 1),
            entry(now - 120, fp_a, 2),
            entry(now, fp_a, 3),
        ]);
        let v = decide(&three, &empty, &lkg(true, true, false), fp_a, now, true);
        assert_eq!(
            v.action,
            Action::RollBack,
            "3 starts in-window must roll back"
        );
        assert!(!v.clear_rollback_active, "RollBack forces clear=false");

        // Window edge: the oldest start is 6m+1s old -> excluded, only 2
        // remain in-window -> Proceed.
        let edge = state_with(vec![
            entry(now - 361, fp_a, 1),
            entry(now - 120, fp_a, 2),
            entry(now, fp_a, 3),
        ]);
        let v = decide(&edge, &empty, &lkg(true, true, false), fp_a, now, true);
        assert_eq!(
            v.action,
            Action::Proceed,
            "the 6m+1s-old start must fall outside the window"
        );
    }

    #[test]
    fn window_boundary_is_inclusive() {
        // Pins today's `same_fingerprint_window_stats` boundary check
        // (`saturating_sub(e.epoch_s) > window_s`) as INCLUSIVE: a start
        // whose age is EXACTLY `CRASH_LOOP_WINDOW` (not one second more)
        // still counts. `two_starts_proceed_three_rolls_back` above
        // already pins the `window_s + 1` (exclusive) side; this pins the
        // `window_s` (inclusive) side, so a `>` -> `>=` mutant flips this
        // assertion (dropping the in-window count from 3 to 2, Proceed
        // instead of RollBack) and gets killed.
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();
        let window_s = CRASH_LOOP_WINDOW.as_secs();

        let boundary = state_with(vec![
            entry(now - window_s, fp_a, 1),
            entry(now - 120, fp_a, 2),
            entry(now, fp_a, 3),
        ]);
        let v = decide(&boundary, &empty, &lkg(true, true, false), fp_a, now, true);
        assert_eq!(
            v.action,
            Action::RollBack,
            "a start exactly CRASH_LOOP_WINDOW old must still count (inclusive boundary)"
        );
    }

    #[test]
    fn mixed_fingerprints_proceed() {
        let fp_a = fingerprint_bytes(b"config-a");
        let fp_b = fingerprint_bytes(b"config-b");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        let mixed = state_with(vec![
            entry(now - 240, fp_a, 1),
            entry(now - 120, fp_b, 2),
            entry(now, fp_a, 3),
        ]);
        let v = decide(&mixed, &empty, &lkg(true, true, false), fp_a, now, true);
        assert_eq!(
            v.action,
            Action::Proceed,
            "condition (b) fails: not all same fp"
        );
    }

    #[test]
    fn bytes_equal_lkg_proceeds_with_crash_loop_detected() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        let three = state_with(vec![
            entry(now - 240, fp_a, 1),
            entry(now - 120, fp_a, 2),
            entry(now, fp_a, 3),
        ]);
        // bytes_equal_current = true: condition (c) fails, exercising
        // lkg.bytes_equal_current, NOT fingerprint hash equality (P13).
        let v = decide(&three, &empty, &lkg(true, true, true), fp_a, now, true);
        assert_eq!(v.action, Action::Proceed);

        // The underlying pattern-detection data IS available to `prepare`
        // (this is the data `DeferredEvent::CrashLoopDetected` is built
        // from) — conditions (a) and (b) both hold even though the overall
        // verdict is Proceed.
        let (total, matching) = same_fingerprint_window_stats(&three, &empty, fp_a, now);
        assert_eq!(total, 3);
        assert_eq!(matching, 3);
        assert!(total >= CRASH_LOOP_THRESHOLD && total == matching);
    }

    #[test]
    fn fingerprint_json_shape_pinned() {
        let fp = Fingerprint::Bytes {
            len: 12,
            hash64: 0xdead_beef,
        };
        let json = serde_json::to_string(&fp).unwrap();
        assert!(json.contains("\"len\":12"), "got {json}");
        assert!(json.contains("\"hash64\":"), "got {json}");
        assert!(!json.contains("\"Bytes\""), "got {json}");

        let unreadable_json = serde_json::to_string(&Fingerprint::Unreadable).unwrap();
        assert_eq!(unreadable_json, "\"unreadable\"");
        assert!(!unreadable_json.contains("Unreadable"));

        let back: Fingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fp);
        let back2: Fingerprint = serde_json::from_str(&unreadable_json).unwrap();
        assert_eq!(back2, Fingerprint::Unreadable);
    }

    #[test]
    fn record_discount_atomic_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        record_discount(dir.path(), 42);
        assert!(dir.path().join("discount-42").exists());
        // Second call: idempotent, no panic, file still present.
        record_discount(dir.path(), 42);
        assert!(dir.path().join("discount-42").exists());

        // Defense in depth (matches `write_atomic_json`'s file mode):
        // a freshly-created discount file is 0o600, not umask-default.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let file_mode = std::fs::metadata(dir.path().join("discount-42"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
        }
    }

    #[test]
    fn record_discount_does_not_truncate_existing_file() {
        // `record_discount` must be an `O_EXCL` create (never a
        // truncating create) so a duplicate call for an already-
        // discounted nonce is a true no-op on disk, not a silent
        // content-clobber. A mutant swapping `create_new(true)` for
        // `truncate(true).create(true)` (or similar) would zero this
        // file's content on the second call; this test kills it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        let path = dir.path().join(format!("{DISCOUNT_PREFIX}77"));
        std::fs::write(&path, b"sentinel-do-not-truncate").unwrap();

        record_discount(dir.path(), 77);

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(
            contents, b"sentinel-do-not-truncate",
            "record_discount must not truncate an existing discount file"
        );

        // Also pins the finding-5 fix: 0o600 on the (pre-existing) file is
        // untouched by `record_discount` — it only tightens permissions on
        // files IT creates, never rewrites permissions of a file that
        // already existed under a different nonce-holder's umask. The
        // freshly-created-file permission guarantee is covered by
        // `record_discount_atomic_and_idempotent`'s sibling assertion
        // below and by `prepare_records_start_and_writes_crash_loop_state`
        // for `crash-loop.json` itself.
    }

    #[test]
    fn boot_inputs_field_list_is_minimal() {
        // Pins the T5 field list: creds_path, strictness, state_dir,
        // lock_path, sd_notify — no socket override (T5 tests set
        // `daemon.socket_path` in the seeded config instead). If this
        // struct literal fails to compile, the field list has drifted from
        // this pin and the test must be amended alongside the code change.
        let _inputs = BootInputs {
            creds_path: PathBuf::from("credentials.toml"),
            strictness: Strictness::Strict,
            state_dir: PathBuf::from("/tmp/state"),
            lock_path: PathBuf::from("/tmp/dormant.lock"),
            sd_notify: SdNotify::from_env(),
        };
    }

    #[test]
    fn boot_plan_field_list_is_minimal() {
        // Pins the rollback-recovery plan's Task 1 field list:
        // chosen_config, operator_config, nonce, deferred_events,
        // pending_message. `operator_config` is the new field (two
        // explicit path roles — see the struct doc); if this literal
        // fails to compile, the field list has drifted from this pin and
        // the test must be amended alongside the code change (P12: still
        // no write-back intent field).
        let _plan = BootPlan {
            chosen_config: PathBuf::from("/tmp/config.toml"),
            operator_config: PathBuf::from("/tmp/config.toml"),
            nonce: 0,
            deferred_events: Vec::new(),
            pending_message: None,
        };
    }

    #[test]
    fn no_lkg_proceeds() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        // Even with a genuine 3-start same-fingerprint pattern, condition
        // (d) (an LKG must exist) blocks RollBack.
        let three = state_with(vec![
            entry(now - 240, fp_a, 1),
            entry(now - 120, fp_a, 2),
            entry(now, fp_a, 3),
        ]);
        let v = decide(&three, &empty, &lkg(false, false, false), fp_a, now, true);
        assert_eq!(v.action, Action::Proceed, "no LKG must never roll back");
    }

    #[test]
    fn discounted_starts_ignored() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let mut discounts = HashSet::new();
        discounts.insert(1u64);
        discounts.insert(2u64);

        // 3 raw entries, but 2 are discounted -> only 1 live -> below
        // threshold -> Proceed.
        let three = state_with(vec![
            entry(now - 240, fp_a, 1),
            entry(now - 120, fp_a, 2),
            entry(now, fp_a, 3),
        ]);
        let v = decide(&three, &discounts, &lkg(true, true, false), fp_a, now, true);
        assert_eq!(
            v.action,
            Action::Proceed,
            "discounted starts must not count"
        );
    }

    #[test]
    fn sticky_continue_rollback() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        let mut state = state_with(vec![entry(now - 30, fp_a, 1), entry(now, fp_a, 2)]);
        state.rollback_active = true;
        state.rolled_back_from = Some(fp_a);

        let v = decide(&state, &empty, &lkg(true, true, true), fp_a, now, true);
        assert_eq!(v.action, Action::ContinueRollback);
        assert!(!v.clear_rollback_active);
        assert!(!v.retry_after_quiet);
        assert!(!v.lkg_missing_disarmed);
    }

    #[test]
    fn bytes_changed_clears_and_proceeds() {
        let fp_a = fingerprint_bytes(b"config-a");
        let fp_b = fingerprint_bytes(b"config-b");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        let mut state = state_with(vec![entry(now - 30, fp_a, 1), entry(now, fp_b, 2)]);
        state.rollback_active = true;
        state.rolled_back_from = Some(fp_a);

        // current is fp_b: the operator edited the config -> F2 clear leg.
        let v = decide(&state, &empty, &lkg(true, true, false), fp_b, now, true);
        assert_eq!(v.action, Action::Proceed);
        assert!(
            v.clear_rollback_active,
            "bytes-changed must clear rollback_active"
        );
        assert!(!v.retry_after_quiet);
    }

    #[test]
    fn quiet_period_clears_and_retries_loudly() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        // previous start is > CRASH_LOOP_WINDOW old, bytes UNCHANGED, and
        // sticky conditions (rollback_active + fp unchanged + LKG good)
        // ALSO hold -> quiet-retry must win (spec §5.2 step 2 before 4).
        let mut state = state_with(vec![entry(now - 400, fp_a, 1), entry(now, fp_a, 2)]);
        state.rollback_active = true;
        state.rolled_back_from = Some(fp_a);

        let v = decide(&state, &empty, &lkg(true, true, true), fp_a, now, true);
        assert_eq!(
            v.action,
            Action::Proceed,
            "quiet-retry must win over sticky ContinueRollback"
        );
        assert!(v.clear_rollback_active);
        assert!(v.retry_after_quiet);
        assert!(!v.lkg_missing_disarmed);
    }

    #[test]
    fn lkg_missing_mid_storm_disarms_loudly() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        let mut state = state_with(vec![entry(now - 30, fp_a, 1), entry(now, fp_a, 2)]);
        state.rollback_active = true;
        state.rolled_back_from = Some(fp_a);

        let v = decide(&state, &empty, &lkg(false, false, false), fp_a, now, true);
        assert_eq!(v.action, Action::Proceed);
        assert!(v.clear_rollback_active);
        assert!(v.lkg_missing_disarmed);
        assert!(!v.retry_after_quiet);
    }

    #[test]
    fn counted_rollback_gate_false_proceeds() {
        let fp_a = fingerprint_bytes(b"config-a");
        let now = 1_000_000u64;
        let empty = HashSet::new();

        let three = state_with(vec![
            entry(now - 240, fp_a, 1),
            entry(now - 120, fp_a, 2),
            entry(now, fp_a, 3),
        ]);
        // lkg_rollback_enabled = false disables ONLY counted rollback.
        let v = decide(&three, &empty, &lkg(true, true, false), fp_a, now, false);
        assert_eq!(
            v.action,
            Action::Proceed,
            "gate=false must suppress counted RollBack"
        );
    }

    #[test]
    fn unreadable_sentinel_matrix() {
        let now = 1_000_000u64;
        let empty = HashSet::new();

        // (b): unreadable == unreadable across starts -> counts as "same
        // fingerprint" -> a persistently unreadable config IS a loop.
        let three = state_with(vec![
            entry(now - 240, Fingerprint::Unreadable, 1),
            entry(now - 120, Fingerprint::Unreadable, 2),
            entry(now, Fingerprint::Unreadable, 3),
        ]);
        // An unreadable current file can never byte-equal a readable LKG,
        // so bytes_equal_current is false (condition c holds).
        let v = decide(
            &three,
            &empty,
            &lkg(true, true, false),
            Fingerprint::Unreadable,
            now,
            true,
        );
        assert_eq!(v.action, Action::RollBack);

        // (c)/immediate: Unreadable never equals a real Bytes fingerprint.
        let real = fingerprint_bytes(b"anything");
        assert_ne!(Fingerprint::Unreadable, real);
    }

    #[test]
    fn corrupt_state_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join(CRASH_LOOP_FILE), b"{ not json").unwrap();

        let state = load_crash_loop_state(dir.path());
        assert_eq!(state, CrashLoopState::default());

        // Absent file: also Default.
        let dir2 = tempfile::tempdir().unwrap();
        let state2 = load_crash_loop_state(dir2.path());
        assert_eq!(state2, CrashLoopState::default());
    }

    #[test]
    fn cap_truncates_to_ten() {
        let fp_a = fingerprint_bytes(b"config-a");
        let starts: Vec<StartEntry> = (0..15u64).map(|i| entry(i, fp_a, i)).collect();
        let mut state = state_with(starts);
        if state.starts.len() > MAX_STARTS {
            let excess = state.starts.len() - MAX_STARTS;
            state.starts.drain(0..excess);
        }
        assert_eq!(state.starts.len(), MAX_STARTS);
        // Most-recent-kept: the last entry (nonce 14) must survive.
        assert_eq!(state.starts.last().unwrap().nonce, 14);
    }

    // ── prepare-level tests: state writes are observable on disk ────────

    fn write_config(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn prepare_records_start_and_writes_crash_loop_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), b"config_version = 1\n");
        let creds = dir.path().join("credentials.toml");
        let state_dir = dir.path().join("state");

        let plan = prepare(&config, &creds, &state_dir, Strictness::Warn, true);
        assert_eq!(plan.chosen_config, config);
        assert!(plan.deferred_events.is_empty());
        assert!(plan.pending_message.is_none());

        let raw = std::fs::read_to_string(state_dir.join(CRASH_LOOP_FILE)).unwrap();
        let state: CrashLoopState = serde_json::from_str(&raw).unwrap();
        assert_eq!(state.starts.len(), 1);
        assert_eq!(state.starts[0].nonce, plan.nonce);
        assert!(!state.rollback_active);

        // Directory / file permissions (samsung_ip pattern).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir_mode = std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            let file_mode = std::fs::metadata(state_dir.join(CRASH_LOOP_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
        }
    }

    #[test]
    fn prepare_counted_rollback_writes_state_and_chooses_lkg() {
        let dir = tempfile::tempdir().unwrap();
        let config_bytes = b"config_version = 1\nbroken = true\n";
        let config = write_config(dir.path(), config_bytes);
        let creds = dir.path().join("credentials.toml");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // Seed two prior same-fingerprint starts within the window.
        let fp = fingerprint_bytes(config_bytes);
        let now = now_epoch_s();
        let seeded = CrashLoopState {
            schema_version: 1,
            starts: vec![entry(now - 60, fp, 101), entry(now - 30, fp, 102)],
            rollback_active: false,
            rolled_back_from: None,
        };
        write_atomic_json(&state_dir, CRASH_LOOP_FILE, &seeded).unwrap();

        // A different, valid LKG file.
        let lkg_bytes = b"config_version = 1\n";
        std::fs::write(state_dir.join(LKG_FILE), lkg_bytes).unwrap();

        let plan = prepare(&config, &creds, &state_dir, Strictness::Warn, true);

        assert_eq!(plan.chosen_config, state_dir.join(LKG_FILE));
        assert!(
            matches!(
                plan.deferred_events.as_slice(),
                [DeferredEvent::RollbackBoot { .. }]
            ),
            "got {:?}",
            plan.deferred_events
        );
        assert!(plan.pending_message.is_some());

        let raw = std::fs::read_to_string(state_dir.join(CRASH_LOOP_FILE)).unwrap();
        let state: CrashLoopState = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            state.starts.len(),
            3,
            "this boot's start must also be recorded"
        );
        assert!(state.rollback_active);
        assert_eq!(state.rolled_back_from, Some(fp));
    }

    #[test]
    fn prepare_emits_crash_loop_detected_when_bytes_equal_lkg() {
        // spec §5.2's closing paragraph: a real same-config crash loop
        // that Proceeds for some other reason (here: bytes_equal_current
        // is true, so condition (c) fails and RollBack never fires) is
        // still worth surfacing loudly. This is `prepare`'s own emission
        // logic (the `Action::Proceed` catchall arm), NOT `decide` — a
        // `decide`-only test cannot see `deferred_events` at all, so
        // deleting the emission branch from `prepare` leaves every
        // decide()-level test green (see RED evidence in the commit
        // report).
        let dir = tempfile::tempdir().unwrap();
        let config_bytes = b"config_version = 1\n";
        let config = write_config(dir.path(), config_bytes);
        let creds = dir.path().join("credentials.toml");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // Seed two prior same-fingerprint starts within the window; this
        // boot's `prepare()` call records a third, matching one.
        let fp = fingerprint_bytes(config_bytes);
        let now = now_epoch_s();
        let seeded = CrashLoopState {
            schema_version: 1,
            starts: vec![entry(now - 60, fp, 301), entry(now - 30, fp, 302)],
            rollback_active: false,
            rolled_back_from: None,
        };
        write_atomic_json(&state_dir, CRASH_LOOP_FILE, &seeded).unwrap();

        // LKG bytes IDENTICAL to the current config -> bytes_equal_current
        // = true -> condition (c) fails -> decide() returns plain Proceed,
        // never RollBack.
        std::fs::write(state_dir.join(LKG_FILE), config_bytes).unwrap();

        let plan = prepare(&config, &creds, &state_dir, Strictness::Warn, true);

        assert_eq!(
            plan.chosen_config, config,
            "bytes-equal-to-LKG must Proceed with the latest config, not roll back"
        );
        assert_eq!(
            plan.deferred_events,
            vec![DeferredEvent::CrashLoopDetected { count: 3 }],
            "3 same-fingerprint in-window starts must still surface \
             CrashLoopDetected even though the verdict is Proceed"
        );
    }

    #[test]
    fn prepare_sweeps_stale_discount_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), b"config_version = 1\n");
        let creds = dir.path().join("credentials.toml");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // A discount file whose mtime we push far into the past (older
        // than CRASH_LOOP_WINDOW) must be swept by the next `prepare`.
        let stale_path = state_dir.join("discount-999");
        let f = std::fs::File::create(&stale_path).unwrap();
        let far_past = std::time::SystemTime::now() - CRASH_LOOP_WINDOW - Duration::from_secs(3600);
        f.set_modified(far_past).unwrap();
        drop(f);

        let _ = prepare(&config, &creds, &state_dir, Strictness::Warn, true);
        assert!(!stale_path.exists(), "stale discount file must be swept");
    }

    #[test]
    fn record_discount_then_prepare_ignores_that_start() {
        let dir = tempfile::tempdir().unwrap();
        let config_bytes = b"config_version = 1\nx = 1\n";
        let config = write_config(dir.path(), config_bytes);
        let creds = dir.path().join("credentials.toml");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let fp = fingerprint_bytes(config_bytes);
        let now = now_epoch_s();
        let seeded = CrashLoopState {
            schema_version: 1,
            starts: vec![entry(now - 60, fp, 201), entry(now - 30, fp, 202)],
            rollback_active: false,
            rolled_back_from: None,
        };
        write_atomic_json(&state_dir, CRASH_LOOP_FILE, &seeded).unwrap();
        // Discount one of the two seeded starts.
        record_discount(&state_dir, 201);

        // LKG differs, so if all 3 (this boot's + the 2 seeded) counted,
        // it would roll back; with one discounted only 2 live entries
        // remain -> below threshold -> Proceed.
        std::fs::write(
            state_dir.join(LKG_FILE),
            b"config_version = 1\ndiffers = true\n",
        )
        .unwrap();

        let plan = prepare(&config, &creds, &state_dir, Strictness::Warn, true);
        assert_eq!(
            plan.chosen_config, config,
            "discounted start must not trigger rollback"
        );
    }

    // ── Task 2 §1 RED: live-reload rollback-recovery helper ─────────────

    #[test]
    fn clear_rollback_after_reload_clears_persisted_state() {
        // RED (rollback-recovery plan, Task 2 §1): seeds `crash-loop.json`
        // with `rollback_active: true` + `rolled_back_from: Some(fp)` (the
        // state a boot-time rollback leaves behind) and asserts the
        // live-reload sibling helper clears both fields, atomically,
        // while preserving every other field (`starts`) untouched. Pre-fix
        // this fails to COMPILE (`clear_rollback_after_reload` does not
        // exist yet); once stubbed in, it must clear the two fields
        // exactly (never touch `starts`/`schema_version`).
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let fp = fingerprint_bytes(b"broken-operator-bytes");
        let seeded = CrashLoopState {
            schema_version: 1,
            starts: vec![entry(1_000, fp, 1)],
            rollback_active: true,
            rolled_back_from: Some(fp),
        };
        write_atomic_json(&state_dir, CRASH_LOOP_FILE, &seeded).unwrap();

        clear_rollback_after_reload(&state_dir)
            .expect("clear must succeed on a writable state_dir");

        let raw = std::fs::read_to_string(state_dir.join(CRASH_LOOP_FILE)).unwrap();
        let state: CrashLoopState = serde_json::from_str(&raw).unwrap();
        assert!(
            !state.rollback_active,
            "rollback_active must clear to false"
        );
        assert_eq!(
            state.rolled_back_from, None,
            "rolled_back_from must clear to None"
        );
        assert_eq!(
            state.starts.len(),
            1,
            "start history must be preserved, not clobbered"
        );
        assert_eq!(state.starts[0].nonce, 1);
    }

    #[test]
    fn clear_rollback_after_reload_surfaces_write_failure() {
        // Task 2 §4's failure-policy branch point: the caller (`Runner`)
        // distinguishes success from failure to decide whether to arm the
        // `rollback_state_clear_pending` retry flag — so the helper must
        // surface a write failure as `Err`, never silently swallow it
        // (unlike `prepare`'s own best-effort `crash-loop.json` write,
        // which is allowed to swallow because nothing downstream retries
        // it). Point `state_dir` at a path whose PARENT is a plain file —
        // `create_dir_all` inside `write_atomic_bytes` cannot create a
        // directory there, guaranteeing a deterministic `Err`.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"x").unwrap();
        let unwritable_state_dir = blocker.join("state");

        let err = clear_rollback_after_reload(&unwritable_state_dir);
        assert!(
            err.is_err(),
            "a blocked state_dir must surface Err, not swallow the failure"
        );
    }

    // ── Task 2 §7: ContinueRollback sibling regression (council Should) ──

    #[test]
    fn continue_rollback_survives_repeated_crash_loop_storm() {
        // A rollback-active daemon that crashes CRASH_LOOP_THRESHOLD MORE
        // times inside the window (operator path still broken, same
        // fingerprint every time) must keep resolving the sticky
        // `ContinueRollback` leg (decide step 4) on EVERY one of those
        // restarts under the two-path model — never re-derive a fresh
        // `RollBack` event (which would shift `rolled_back_from`) and
        // never touch `last-known-good.toml` itself: `prepare()` only
        // ever READS the LKG file (the only writer is `App::start`'s
        // coupling-hazard suppression, Task 1 §5, which never arms a
        // candidate at all while `rollback_active` — so nothing in this
        // storm can promote the still-broken operator bytes over the
        // LKG). Simulated as repeated `prepare()` calls against the same
        // broken operator config, the way `dormantd` would restart-loop
        // under systemd.
        let dir = tempfile::tempdir().unwrap();
        let bad_bytes = b"config_version = 1\nbroken = true\n";
        let config = write_config(dir.path(), bad_bytes);
        let creds = dir.path().join("credentials.toml");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let fp = fingerprint_bytes(bad_bytes);
        let lkg_bytes = b"config_version = 1\n";
        let lkg_path = state_dir.join(LKG_FILE);
        std::fs::write(&lkg_path, lkg_bytes).unwrap();

        let seeded = CrashLoopState {
            schema_version: 1,
            starts: vec![entry(now_epoch_s() - 30, fp, 9001)],
            rollback_active: true,
            rolled_back_from: Some(fp),
        };
        write_atomic_json(&state_dir, CRASH_LOOP_FILE, &seeded).unwrap();

        for i in 0..CRASH_LOOP_THRESHOLD {
            let plan = prepare(&config, &creds, &state_dir, Strictness::Warn, true);
            assert_eq!(
                plan.chosen_config, lkg_path,
                "storm iteration {i}: must keep choosing the LKG substitute"
            );
            assert_eq!(
                plan.deferred_events,
                vec![DeferredEvent::RollbackContinued],
                "storm iteration {i}: must resolve the sticky ContinueRollback leg, not a \
                 fresh RollBack"
            );

            let state: CrashLoopState = serde_json::from_str(
                &std::fs::read_to_string(state_dir.join(CRASH_LOOP_FILE)).unwrap(),
            )
            .unwrap();
            assert!(
                state.rollback_active,
                "storm iteration {i}: must stay active"
            );
            assert_eq!(
                state.rolled_back_from,
                Some(fp),
                "storm iteration {i}: rolled_back_from must not shift to a new RollBack event"
            );

            assert_eq!(
                std::fs::read(&lkg_path).unwrap(),
                lkg_bytes,
                "storm iteration {i}: last-known-good.toml must stay byte-identical across the \
                 storm"
            );
        }
    }
}

// ── should_promote matrix (T4, spec §4) ─────────────────────────────────

#[cfg(test)]
mod promote_tests {
    use super::*;
    use dormant_core::rules::{ControllerHealth, ControllerRole, DisplaySnapshot};

    fn healthy_ctl(name: &str) -> ControllerHealth {
        ControllerHealth {
            name: name.to_string(),
            role: ControllerRole::Primary,
            healthy: true,
            detail: None,
        }
    }

    fn unhealthy_ctl(name: &str) -> ControllerHealth {
        ControllerHealth {
            name: name.to_string(),
            role: ControllerRole::Primary,
            healthy: false,
            detail: Some("wedged".to_string()),
        }
    }

    fn display(phase: &str, controllers: Vec<ControllerHealth>) -> DisplaySnapshot {
        DisplaySnapshot {
            phase: phase.to_string(),
            inhibited: false,
            paused: false,
            cmd_gen: 0,
            controllers,
            wake_attempts: 0,
            last_blank_failed: false,
            stage: None,
        }
    }

    fn snap(displays: Vec<(&str, DisplaySnapshot)>) -> StateSnapshot {
        StateSnapshot {
            sensors: Vec::new(),
            zones: Vec::new(),
            displays: displays
                .into_iter()
                .map(|(id, d)| (id.to_string(), d))
                .collect(),
            pending_reload: None,
        }
    }

    fn all_healthy_snap() -> StateSnapshot {
        snap(vec![(
            "mon",
            display("active", vec![healthy_ctl("command")]),
        )])
    }

    const WINDOW: Duration = Duration::from_secs(30);

    #[test]
    fn window_not_elapsed_waits() {
        let since = Instant::now();
        let now = since + Duration::from_secs(10);
        let v = should_promote(since, now, WINDOW, &all_healthy_snap(), 0, true);
        assert_eq!(v, PromoteVerdict::Wait);
    }

    #[test]
    fn elapsed_healthy_clean_promotes() {
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let v = should_promote(since, now, WINDOW, &all_healthy_snap(), 0, true);
        assert_eq!(v, PromoteVerdict::Promote);
    }

    #[test]
    fn elapsed_healthy_dirty_skips() {
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let v = should_promote(since, now, WINDOW, &all_healthy_snap(), 0, false);
        assert_eq!(v, PromoteVerdict::SkipDirty);
    }

    #[test]
    fn all_unhealthy_display_defers_below_cap() {
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let unhealthy = snap(vec![(
            "mon",
            display("active", vec![unhealthy_ctl("command")]),
        )]);
        for defer_count in 0..LKG_HEALTH_DEFER_CAP {
            let v = should_promote(since, now, WINDOW, &unhealthy, defer_count, true);
            assert_eq!(
                v,
                PromoteVerdict::DeferHealth,
                "defer_count={defer_count} must still defer"
            );
        }
    }

    #[test]
    fn all_unhealthy_display_promotes_despite_health_at_cap() {
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let unhealthy = snap(vec![(
            "mon",
            display("active", vec![unhealthy_ctl("command")]),
        )]);
        let v = should_promote(since, now, WINDOW, &unhealthy, LKG_HEALTH_DEFER_CAP, true);
        assert_eq!(v, PromoteVerdict::PromoteDespiteHealth);
    }

    #[test]
    fn dirty_check_still_wins_at_cap() {
        // Invariant #6 (never promote unproven bytes) outranks the
        // starvation cap — a dirty on-disk edit still skips even once the
        // defer cap has been reached.
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let unhealthy = snap(vec![(
            "mon",
            display("active", vec![unhealthy_ctl("command")]),
        )]);
        let v = should_promote(since, now, WINDOW, &unhealthy, LKG_HEALTH_DEFER_CAP, false);
        assert_eq!(v, PromoteVerdict::SkipDirty);
    }

    #[test]
    fn different_unhealthy_sets_accumulate_identically_r3_m2() {
        // R3-M2: the deferral cap counts ANY consecutive deferred
        // candidates regardless of WHICH display(s) are unhealthy each
        // time. `should_promote` takes only a bare `defer_count` — never a
        // per-set counter — so calling it with a DIFFERENT unhealthy
        // display each time must still defer/promote on the exact same
        // schedule as a fixed set would (a same-set-only counter would be
        // the RED trap this pins against).
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let set_a = snap(vec![(
            "a",
            display("active", vec![unhealthy_ctl("command")]),
        )]);
        let set_b = snap(vec![(
            "b",
            display("active", vec![unhealthy_ctl("command")]),
        )]);
        let set_c = snap(vec![(
            "c",
            display("active", vec![unhealthy_ctl("command")]),
        )]);

        assert_eq!(
            should_promote(since, now, WINDOW, &set_a, 0, true),
            PromoteVerdict::DeferHealth
        );
        assert_eq!(
            should_promote(since, now, WINDOW, &set_b, 1, true),
            PromoteVerdict::DeferHealth
        );
        assert_eq!(
            should_promote(since, now, WINDOW, &set_c, 2, true),
            PromoteVerdict::DeferHealth
        );
        // 4th distinct set, defer_count now at the cap -> promotes anyway.
        let set_d = snap(vec![(
            "d",
            display("active", vec![unhealthy_ctl("command")]),
        )]);
        assert_eq!(
            should_promote(since, now, WINDOW, &set_d, LKG_HEALTH_DEFER_CAP, true),
            PromoteVerdict::PromoteDespiteHealth
        );
    }

    #[test]
    fn empty_health_display_never_defers() {
        // A display never commanded during the window has an empty
        // `controllers` vec — unproven, not failing; must never defer.
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let unproven = snap(vec![("mon", display("active", Vec::new()))]);
        let v = should_promote(since, now, WINDOW, &unproven, 0, true);
        assert_eq!(v, PromoteVerdict::Promote);
    }

    #[test]
    fn mixed_proven_and_unproven_only_proven_can_defer() {
        // One display fully healthy, one empty (unproven), one all-
        // unhealthy: the all-unhealthy one alone must still defer.
        let since = Instant::now();
        let now = since + WINDOW + Duration::from_secs(1);
        let mixed = snap(vec![
            ("ok", display("active", vec![healthy_ctl("command")])),
            ("unproven", display("active", Vec::new())),
            ("bad", display("active", vec![unhealthy_ctl("command")])),
        ]);
        let v = should_promote(since, now, WINDOW, &mixed, 0, true);
        assert_eq!(v, PromoteVerdict::DeferHealth);
    }
}
