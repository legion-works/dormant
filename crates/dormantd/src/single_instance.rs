//! Per-user-session single-instance flock guard.
//!
//! Acquires an exclusive advisory lock on a fixed per-session path so that two
//! `dormantd` instances cannot start for the same user — they would fight the
//! same physical displays' DDC bus. The socket connect-test guard in
//! [`crate::ipc::spawn`] only blocks a second daemon on the SAME socket path;
//! a different `XDG_RUNTIME_DIR` or a custom `socket_path` config bypasses it.
//!
//! The lock path is deliberately NOT config-overridable — the whole point is
//! that it cannot be sidestepped the way the socket path can.
//!
//! The kernel releases the lock when the process exits (even crash), so there
//! is no stale-lock cleanup problem.

#[cfg(unix)]
use std::io::Write;

use std::path::Path;

use anyhow::Context;

/// RAII guard holding an exclusive advisory lock on the per-user-session lock
/// file. Dropping the guard releases the lock (kernel-enforced on process exit
/// as well — crash-safe).
#[derive(Debug)]
pub struct SingleInstanceLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

/// Acquire the per-user-session single-instance lock.
///
/// Creates the parent directory if needed, opens (or creates) the lock file,
/// and takes an exclusive non-blocking advisory lock on it. On success, writes
/// the current PID into the file (best-effort, informational only — the flock
/// is the real guard) and returns a [`SingleInstanceLock`] that must be held
/// for the daemon's entire lifetime.
///
/// # Errors
///
/// Returns an error if another daemon already holds the lock or if the lock
/// file cannot be created/opened.
pub fn acquire(lock_path: &Path) -> anyhow::Result<SingleInstanceLock> {
    acquire_impl(lock_path)
}

#[cfg(unix)]
fn acquire_impl(lock_path: &Path) -> anyhow::Result<SingleInstanceLock> {
    use std::io::ErrorKind;

    // Ensure parent directory exists (XDG_RUNTIME_DIR is 0o700, but we may
    // fall back to /run/dormant/ which needs creation).
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create lock parent directory '{}'", parent.display()))?;
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("open lock file '{}'", lock_path.display()))?;

    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    // SAFETY: fd is a valid open file descriptor.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::WouldBlock {
            anyhow::bail!(
                "another dormant instance is already running for this user session (lock held on '{}')",
                lock_path.display()
            );
        }
        return Err(err)
            .with_context(|| format!("acquire flock on lock file '{}'", lock_path.display()));
    }

    // Write PID into the lock file — best-effort (flock is the real guard).
    // A truncated file is harmless; the lock still holds.
    let _ = writeln!(&file, "{}", std::process::id());

    tracing::info!(
        event = "single_instance_locked",
        path = %lock_path.display(),
    );

    Ok(SingleInstanceLock { _file: file })
}

#[cfg(not(unix))]
fn acquire_impl(lock_path: &Path) -> anyhow::Result<SingleInstanceLock> {
    // Windows has no flock; dormantd's real runtime is unix. This stub keeps
    // cross-compile green — the guard is a no-op.
    let _ = lock_path;
    tracing::warn!(
        event = "single_instance_lock_unavailable",
        "single-instance lock is not available on this platform",
    );
    Ok(SingleInstanceLock {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        #[test]
        fn acquire_succeeds_on_empty_path() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("dormant.lock");
            let guard = acquire(&lock_path).unwrap();
            assert!(lock_path.exists());
            drop(guard);
        }

        #[test]
        fn second_acquire_fails_while_held() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("dormant.lock");
            let _guard = acquire(&lock_path).unwrap();
            let result = acquire(&lock_path);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("already running"),
                "expected 'already running' in error, got: {err}"
            );
        }

        #[test]
        fn acquire_succeeds_after_guard_dropped() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("dormant.lock");
            let guard = acquire(&lock_path).unwrap();
            drop(guard);
            // Should succeed — lock released on drop.
            let guard2 = acquire(&lock_path).unwrap();
            drop(guard2);
        }
    }
}
