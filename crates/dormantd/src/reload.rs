//! Config-file watching for hot reload.
//!
//! The actual rebuild lives in the app's run loop (`Runner::reload`); this
//! module only sets up the filesystem watcher that pokes the run loop when the
//! config file changes. SIGHUP is wired directly in the run loop.
//!
//! We watch the config file's **parent directory** (not the file inode)
//! because editors and `install(1)` frequently replace the file via
//! rename, which detaches an inode-level watch. Events are filtered down to
//! the config path and coalesced into a unit tick.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// A live config watcher. Dropping it stops watching.
pub struct ConfigWatcher {
    /// Held to keep the OS watch alive for the process lifetime.
    _watcher: notify::RecommendedWatcher,
    /// Ticks (one per relevant filesystem change).
    pub rx: mpsc::Receiver<()>,
}

/// Start watching `config_path` for modify/create events.
///
/// # Errors
///
/// Returns an error if the watcher cannot be created or the parent directory
/// cannot be watched.
pub fn config_watcher(config_path: &Path) -> Result<ConfigWatcher> {
    let (tx, rx) = mpsc::channel(8);

    let target: PathBuf = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let target_name = config_path.file_name().map(std::ffi::OsString::from);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        // Fire only for the file we care about. If the event carries no
        // paths, fire anyway (better a spurious reload than a missed one).
        let relevant = event.paths.is_empty()
            || event.paths.iter().any(|p| {
                p == &target
                    || (target_name.is_some()
                        && p.file_name().map(std::ffi::OsString::from) == target_name)
            });
        if relevant {
            let _ = tx.blocking_send(());
        }
    })
    .context("create config watcher")?;

    let watch_dir = config_path.parent().filter(|p| !p.as_os_str().is_empty());
    let watch_dir = watch_dir.unwrap_or_else(|| Path::new("."));
    watcher
        .watch(watch_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch config directory '{}'", watch_dir.display()))?;

    Ok(ConfigWatcher {
        _watcher: watcher,
        rx,
    })
}
