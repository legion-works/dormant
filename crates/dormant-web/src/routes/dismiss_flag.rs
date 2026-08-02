//! Shared persistence helper for "X dismissed" flag files.
//!
//! `routes::star_nudge` and `routes::wear_sampling` both write a sibling
//! flag file in the config directory to remember that the operator has
//! dismissed a one-time UI nudge. The two write paths were initially
//! copy-pasted verbatim — this module is the single home of the
//! atomic-write discipline so a future change (SEC review, perf,
//! platform tweak) lands in one place.
//!
//! Atomicity contract (SEC S3 / preserved-by-construction):
//! - The temp file is opened with `create_new(true)` — fails on an
//!   existing file or symlink, preventing a predictable-name symlink
//!   attack. A stale tmp left by a prior crash is removed once and retried.
//! - Write+sync, then `drop` before `rename` (Windows holds the file
//!   for the rename otherwise).
//! - Unix permissions are forced to 0o644 so the daemon can read the
//!   flag even if the operator runs `dormantd` under a different user.
//!
//! The log event name is a LITERAL `&str` passed by each caller
//! (e.g. `"star_nudge_dismissed"`, `"wear_sampling_nudge_dismissed"`)
//! — never `format!`-constructed. Per AGENTS.md rule 3, log event names
//! are literal strings at the definition site so `grep 'event = "..."'`
//! finds every emitter; a derived name is invisible to that search and
//! would also silently double the suffix when the flag file already
//! ends in `-dismissed`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::error::WebError;

/// Write the dismiss-flag file at `path` atomically (tempfile + rename).
/// Idempotent: if the file already exists this is a no-op.
///
/// `event` is the literal log event name to emit on success — pass a
/// `&'static str` constant from the call site so a `grep` for
/// `event = "..."` finds the emitter. The temp filename is derived
/// from `path.file_name()` + `.tmp` in the same directory (or `.` if
/// `path` has no parent).
///
/// # Errors
///
/// All I/O failures are surfaced as [`WebError::ConfigReadError`] so
/// the `/api/...` route handlers can simply propagate.
pub(crate) fn write_dismiss_flag(path: &Path, event: &'static str) -> Result<(), WebError> {
    if path.exists() {
        return Ok(());
    }

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dismiss-flag");

    // CORR 3: if parent is None or empty (e.g. relative `config.toml`),
    // fall back to the current directory so the flag file still lands
    // somewhere reachable rather than panicking.
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tmp_path = dir.join(format!("{file_name}.tmp"));

    let mut f = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Stale tmp from a prior crash — remove and retry once.
            let _ = std::fs::remove_file(&tmp_path);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .map_err(|e2| {
                    WebError::ConfigReadError(format!(
                        "cannot create {file_name} temp after stale cleanup: {e2}"
                    ))
                })?
        }
        Err(e) => {
            return Err(WebError::ConfigReadError(format!(
                "cannot create {file_name} temp: {e}"
            )));
        }
    };
    f.write_all(b"dismissed\n")
        .and_then(|()| f.sync_all())
        .map_err(|e| WebError::ConfigReadError(format!("cannot write {file_name} temp: {e}")))?;
    drop(f);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&tmp_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o644);
            let _ = std::fs::set_permissions(&tmp_path, perms);
        }
    }
    std::fs::rename(&tmp_path, path)
        .map_err(|e| WebError::ConfigReadError(format!("cannot rename {file_name} temp: {e}")))?;
    tracing::info!(event = event, ?path);
    Ok(())
}
