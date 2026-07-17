//! Gamma-blank breadcrumb — daemon-lifetime crash insurance for the macOS
//! gamma-black blank mechanism (see [`crate::macos_gamma_black`]).
//!
//! ## Why a breadcrumb at all
//!
//! [`crate::macos_gamma_black::GammaHoldRegistry`] is an in-process store:
//! it knows exactly which selectors are currently blanked and what table to
//! replay on wake, but only for as long as the daemon process is alive. If
//! the process dies uncleanly (crash, `kill -9`, power loss) while one or
//! more displays are gamma-blanked, the in-memory hold vanishes with it —
//! the panel is left driving an all-zero gamma table with nothing left
//! alive that remembers what to restore. [`GammaBreadcrumb`] is the on-disk
//! insurance policy: a small marker file, written BEFORE the first LUT
//! write for a selector (never after — see [`GammaBreadcrumb::add_selector`]),
//! that survives the crash and lets the next boot (or a shutdown-path best-
//! effort restore) know a system-wide gamma restore is owed.
//!
//! Deliberately NOT a copy of the LUT data itself — just the stable
//! selectors that are (or were, at last update) held blanked, plus a
//! timestamp. Restoring from a stale breadcrumb goes through the coarse,
//! system-wide `CGDisplayRestoreColorSyncSettings` restore (see
//! `dormantd::gamma_recovery`), not a byte-exact per-selector table replay
//! — by the time this file matters, the in-process saved table is already
//! gone.
//!
//! ## Concurrency
//!
//! [`GammaBreadcrumb`] holds a single process-wide marker [`std::sync::Mutex`]
//! guarding every read-modify-write of the on-disk state, so that two
//! displays blanking concurrently (`add_selector` racing `add_selector`, or
//! `add_selector` racing `remove_selector`) merge into the same
//! [`GammaBreadcrumbState::held_selectors`] set rather than one write
//! clobbering the other's.
//!
//! ## Atomic write shape
//!
//! [`GammaBreadcrumb`] copies the temp-file-then-rename, `0o700` dir /
//! `0o600` file pattern documented at
//! `dormantd::boot_guard::write_atomic_bytes`
//! (`crates/dormantd/src/boot_guard.rs:986-1010`) rather than importing it:
//! `dormant-displays` is a dependency OF `dormantd`, not the other way
//! around, so pulling `dormantd::boot_guard` in here would be a dependency
//! cycle. The shape is intentionally identical so the two on-disk stores
//! (`crash-loop.json` and `gamma-blank.json`) behave the same way under a
//! torn write or a concurrent reader.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Breadcrumb filename inside the resolved state directory
/// (`state_dir()/gamma-blank.json`).
pub const BREADCRUMB_FILENAME: &str = "gamma-blank.json";

/// On-disk breadcrumb shape — versioned JSON, stable selectors + a
/// timestamp, deliberately NOT gamma table data (see module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GammaBreadcrumbState {
    /// Schema version — bump on any incompatible shape change (mirrors
    /// `dormantd::boot_guard::CrashLoopState::schema_version`).
    pub schema_version: u32,
    /// Stable `cg:<uuid>` selectors currently believed gamma-blanked with no
    /// confirmed wake since. A `BTreeSet` (not a `Vec`/`HashSet`) so
    /// concurrent `add_selector` calls from different threads merge
    /// deterministically and so the serialized JSON is stable/diffable.
    pub held_selectors: BTreeSet<String>,
    /// Unix epoch seconds of the last update.
    pub updated_at_epoch_s: u64,
}

impl Default for GammaBreadcrumbState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            held_selectors: BTreeSet::new(),
            updated_at_epoch_s: 0,
        }
    }
}

/// Process-wide breadcrumb marker for one resolved state directory.
///
/// Cheap to construct (holds only a path + a mutex) — callers build one
/// `Arc<GammaBreadcrumb>` per daemon process (mirrors
/// [`crate::macos_gamma_black::GammaHoldRegistry`] and
/// [`crate::ddc_lock::PanelLocks`]'s "one process-wide instance, threaded
/// through every controller" contract) and share it across every
/// `macos-gamma-black` controller instance, across every reload generation.
pub struct GammaBreadcrumb {
    dir: PathBuf,
    /// Guards every read-modify-write below — see module docs
    /// ("Concurrency").
    marker: StdMutex<()>,
}

impl GammaBreadcrumb {
    /// Build a breadcrumb rooted at `state_dir` (the daemon's resolved
    /// state directory — `dormant_core::paths::state_dir()` in production).
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: state_dir.into(),
            marker: StdMutex::new(()),
        }
    }

    /// The breadcrumb file's full path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(BREADCRUMB_FILENAME)
    }

    /// True when the breadcrumb file exists on disk.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path().is_file()
    }

    /// Read the current breadcrumb state, if any.
    ///
    /// Corrupt-tolerant: an absent file, an unreadable file, or JSON that
    /// fails to parse are all treated as "no breadcrumb" (`None`) rather
    /// than propagating an error — mirrors
    /// `dormantd::boot_guard`'s corrupt/torn-file tolerance for
    /// `crash-loop.json` (a breadcrumb this module can't parse is no more
    /// trustworthy than one that never existed, and refusing to boot over a
    /// torn recovery file would be strictly worse than treating it as
    /// absent).
    #[must_use]
    pub fn read(&self) -> Option<GammaBreadcrumbState> {
        let raw = std::fs::read_to_string(self.path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Add `selector` to the held set — called BEFORE the first gamma-table
    /// write for that selector's blank (see
    /// `crate::macos_gamma_black::MacosGammaBlackController::blank`), so
    /// that a crash between this call and the write still leaves a
    /// breadcrumb naming the selector about to go dark, never a write with
    /// no breadcrumb behind it.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the atomic write fails (directory
    /// creation, temp-file write, or rename). Callers (the controller's
    /// `blank()`) should treat a write failure as fatal to the blank
    /// attempt — a gamma-black write with no confirmed breadcrumb behind it
    /// is exactly the crash-recovery gap this module exists to close.
    pub fn add_selector(&self, selector: &str) -> io::Result<()> {
        let _guard = self.lock_marker();
        let mut state = self.read().unwrap_or_default();
        state.held_selectors.insert(selector.to_string());
        state.updated_at_epoch_s = now_epoch_s();
        self.write_atomic(&state)
    }

    /// Remove `selector` from the held set — called AFTER a confirmed,
    /// successful `wake()` replay. Deletes the breadcrumb file entirely
    /// once no selectors remain held (rather than leaving an empty-set
    /// file behind), so [`Self::exists`] is the single source of truth for
    /// "is a system-wide restore owed".
    ///
    /// A no-op (not an error) when no breadcrumb file exists at all, or
    /// when `selector` was never in the held set — `wake()` calling this
    /// unconditionally on every confirmed wake (even one that never
    /// occupied a hold) must never fail.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the underlying atomic write or delete
    /// fails.
    pub fn remove_selector(&self, selector: &str) -> io::Result<()> {
        let _guard = self.lock_marker();
        let Some(mut state) = self.read() else {
            return Ok(());
        };
        state.held_selectors.remove(selector);
        if state.held_selectors.is_empty() {
            self.delete_locked()
        } else {
            state.updated_at_epoch_s = now_epoch_s();
            self.write_atomic(&state)
        }
    }

    /// Unconditionally delete the breadcrumb file (used by startup/shutdown
    /// recovery once a system-wide restore has succeeded). Treats
    /// "already absent" as success, not an error.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] on any removal failure other than
    /// "not found".
    pub fn delete(&self) -> io::Result<()> {
        let _guard = self.lock_marker();
        self.delete_locked()
    }

    fn delete_locked(&self) -> io::Result<()> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn lock_marker(&self) -> std::sync::MutexGuard<'_, ()> {
        self.marker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Atomic write — temp file + rename, dir `0o700` / file `0o600` on
    /// Unix. Copies the shape documented at
    /// `dormantd::boot_guard::write_atomic_bytes`
    /// (`crates/dormantd/src/boot_guard.rs:986-1010`) rather than importing
    /// it — see the module docs' "Atomic write shape" section for why.
    fn write_atomic(&self, state: &GammaBreadcrumbState) -> io::Result<()> {
        use std::io::Write as _;

        let raw = serde_json::to_string_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        }

        let final_path = self.path();
        let tmp_path = self.dir.join(format!("{BREADCRUMB_FILENAME}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp_path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
            }
            f.write_all(raw.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }
}

/// Best-effort current epoch seconds; `0` on a clock error (pre-1970 system
/// clock) rather than panicking — a timestamp this module uses only for
/// diagnostics, never for correctness.
fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Resolve `dir`'s breadcrumb path — a tiny free-function wrapper used by
/// [`GammaBreadcrumb::path`] callers that only have a `&Path`, not a
/// constructed [`GammaBreadcrumb`] (e.g. a startup check before any
/// controller exists).
#[must_use]
pub fn breadcrumb_path(dir: &Path) -> PathBuf {
    dir.join(BREADCRUMB_FILENAME)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn absent_breadcrumb_reads_none_and_does_not_exist() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        assert!(!bc.exists());
        assert_eq!(bc.read(), None);
    }

    #[test]
    fn add_selector_persists_and_marks_existing() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.add_selector("cg:a").unwrap();

        assert!(bc.exists());
        let state = bc.read().expect("breadcrumb present");
        assert_eq!(state.schema_version, 1);
        assert!(state.held_selectors.contains("cg:a"));
        assert_eq!(state.held_selectors.len(), 1);
    }

    #[test]
    fn add_two_selectors_both_present() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.add_selector("cg:a").unwrap();
        bc.add_selector("cg:b").unwrap();

        let state = bc.read().unwrap();
        assert_eq!(
            state.held_selectors,
            BTreeSet::from(["cg:a".to_string(), "cg:b".to_string()])
        );
    }

    #[test]
    fn remove_selector_deletes_file_when_empty() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.add_selector("cg:a").unwrap();
        assert!(bc.exists());

        bc.remove_selector("cg:a").unwrap();
        assert!(
            !bc.exists(),
            "file must be deleted once no selectors remain held"
        );
        assert_eq!(bc.read(), None);
    }

    #[test]
    fn remove_one_of_two_selectors_keeps_file_with_the_other() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.add_selector("cg:a").unwrap();
        bc.add_selector("cg:b").unwrap();

        bc.remove_selector("cg:a").unwrap();

        assert!(bc.exists(), "cg:b is still held; file must survive");
        let state = bc.read().unwrap();
        assert_eq!(state.held_selectors, BTreeSet::from(["cg:b".to_string()]));
    }

    #[test]
    fn remove_selector_with_no_file_is_a_noop() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.remove_selector("cg:never-added").unwrap();
        assert!(!bc.exists());
    }

    #[test]
    fn remove_unknown_selector_from_existing_file_is_a_noop_on_the_others() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.add_selector("cg:a").unwrap();
        bc.remove_selector("cg:not-held").unwrap();

        let state = bc.read().unwrap();
        assert_eq!(state.held_selectors, BTreeSet::from(["cg:a".to_string()]));
    }

    #[test]
    fn delete_removes_file_unconditionally() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.add_selector("cg:a").unwrap();
        bc.add_selector("cg:b").unwrap();

        bc.delete().unwrap();
        assert!(!bc.exists());
    }

    #[test]
    fn delete_on_absent_file_is_ok() {
        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.delete().unwrap();
    }

    #[test]
    fn corrupt_file_is_tolerated_as_absent() {
        let dir = temp_dir();
        std::fs::write(dir.path().join(BREADCRUMB_FILENAME), b"not json{{{").unwrap();
        let bc = GammaBreadcrumb::new(dir.path());
        assert_eq!(bc.read(), None, "corrupt breadcrumb must read as None");
    }

    /// RED-first test 10 (Task 8 plan):
    /// `concurrent_marker_updates_merge_instead_of_losing_a_display` — two
    /// threads adding DIFFERENT selectors concurrently through the SAME
    /// `GammaBreadcrumb` must both land in the final `BTreeSet`, proving the
    /// marker mutex serializes the read-modify-write instead of one thread's
    /// write clobbering the other's (a naive "read, mutate, write" with no
    /// lock would lose whichever thread wrote second's view of the first
    /// thread's addition).
    #[test]
    fn concurrent_marker_updates_merge_instead_of_losing_a_display() {
        let dir = temp_dir();
        let bc = Arc::new(GammaBreadcrumb::new(dir.path()));

        let bc_a = Arc::clone(&bc);
        let t_a = std::thread::spawn(move || bc_a.add_selector("cg:a").unwrap());
        let bc_b = Arc::clone(&bc);
        let t_b = std::thread::spawn(move || bc_b.add_selector("cg:b").unwrap());

        t_a.join().unwrap();
        t_b.join().unwrap();

        let state = bc.read().expect("breadcrumb present after both adds");
        assert!(
            state.held_selectors.contains("cg:a"),
            "cg:a must survive the concurrent update: {state:?}"
        );
        assert!(
            state.held_selectors.contains("cg:b"),
            "cg:b must survive the concurrent update: {state:?}"
        );
        assert_eq!(state.held_selectors.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn file_and_dir_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir();
        let bc = GammaBreadcrumb::new(dir.path());
        bc.add_selector("cg:a").unwrap();

        let file_mode = std::fs::metadata(bc.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "breadcrumb file must be 0600");

        let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "state dir must be 0700");
    }

    #[test]
    fn breadcrumb_path_matches_filename_constant() {
        let dir = temp_dir();
        assert_eq!(
            breadcrumb_path(dir.path()),
            dir.path().join(BREADCRUMB_FILENAME)
        );
        assert_eq!(
            GammaBreadcrumb::new(dir.path()).path(),
            dir.path().join(BREADCRUMB_FILENAME)
        );
    }
}
