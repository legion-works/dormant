//! `dormantctl launchd` — install/uninstall the checked-in macOS
//! `LaunchAgent` plist.
//!
//! `install`/`uninstall`/`installed_paths` are NOT `cfg(target_os =
//! "macos")`-gated: they take an explicit `home` directory and touch only
//! the filesystem beneath it, so they are fully exercisable on Linux (see
//! the tests below). Only [`run`] — the CLI-facing entry point dispatched
//! from `main.rs` — is platform-gated, mirroring the pre-existing
//! `Ddcci`/`Kwin`/`macos-*` doctor-arm pattern in `cmd_doctor.rs`: parsing
//! of `launchd install`/`launchd uninstall` is unconditional (works on
//! every platform's `--help`), only the handler behind it differs.
//!
//! Because `install`/`uninstall`/the embedded plists/etc. are otherwise only
//! reachable from [`run`]'s `cfg(target_os = "macos")` branch, a non-macOS,
//! non-test build of this binary (`cargo clippy --all-targets` checks that
//! exact configuration) never calls them and `-D warnings` would flag them
//! dead. `cfg(test)` already exempts them via the tests below; this
//! module-wide `allow` covers the plain non-test, non-macOS build too.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The daemon `LaunchAgent` plist embedded from the cargo-dist release asset.
pub const EMBEDDED_DAEMON_PLIST: &[u8] =
    include_bytes!("../../dormantd/share/com.legionworks.dormant.plist");

/// The tray `LaunchAgent` plist embedded from the cargo-dist release asset.
pub const EMBEDDED_TRAY_PLIST: &[u8] =
    include_bytes!("../../dormant-tray/share/com.legionworks.dormant-tray.plist");

/// Backward-compatible alias for the embedded daemon plist.
pub const EMBEDDED_PLIST: &[u8] = EMBEDDED_DAEMON_PLIST;

const DAEMON_PLIST_FILE_NAME: &str = "com.legionworks.dormant.plist";
const TRAY_PLIST_FILE_NAME: &str = "com.legionworks.dormant-tray.plist";

/// Canonical filesystem locations for dormant's `LaunchAgents`.
#[derive(Debug, PartialEq, Eq)]
pub struct LaunchdPaths {
    /// Path to the daemon `LaunchAgent` plist.
    pub daemon: PathBuf,
    /// Path to the tray `LaunchAgent` plist.
    pub tray: PathBuf,
}

/// The aggregate result of removing dormant's `LaunchAgent` plists.
#[derive(Debug, PartialEq, Eq)]
pub struct UninstallResult {
    /// The paths considered for removal.
    pub paths: LaunchdPaths,
    /// Whether the daemon plist was present and removed.
    pub daemon_removed: bool,
    /// Whether the tray plist was present and removed.
    pub tray_removed: bool,
}

/// CLI surface for `dormantctl launchd <install|uninstall>`.
#[derive(clap::Subcommand, Debug)]
pub enum LaunchdSubcommand {
    /// Install the checked-in `LaunchAgent` plists to
    /// `~/Library/LaunchAgents/com.legionworks.dormant.plist` and
    /// `~/Library/LaunchAgents/com.legionworks.dormant-tray.plist`.
    ///
    /// Explicit, non-root, idempotent: re-running overwrites both files with
    /// the same embedded bytes (atomically per file — a concurrent
    /// reader/launchctl never observes a partially-written plist) rather than
    /// erroring. Does NOT bootstrap or start either agent; run
    /// `launchctl bootstrap gui/$UID
    /// "$HOME/Library/LaunchAgents/com.legionworks.dormant.plist"` and
    /// `launchctl bootstrap gui/$UID
    /// "$HOME/Library/LaunchAgents/com.legionworks.dormant-tray.plist"`
    /// yourself afterward for the `com.legionworks.dormant` and
    /// `com.legionworks.dormant-tray` labels respectively.
    Install,
    /// Remove the canonical installed `LaunchAgent` plists, if present.
    ///
    /// Only ever removes those two files. Does not bootout either running
    /// launchd label — run `launchctl bootout gui/$UID
    /// com.legionworks.dormant` and `launchctl bootout gui/$UID
    /// com.legionworks.dormant-tray` first if the agents are loaded,
    /// otherwise launchd keeps their last-loaded definitions in memory even
    /// after the on-disk files are gone.
    Uninstall,
}

/// Outcome of [`run`], for `main.rs` to render and exit-code accordingly.
///
/// `Installed`/`Uninstalled` are only ever constructed from the
/// `cfg(target_os = "macos")` half of [`run`]; covered by this file's
/// module-wide `dead_code` allowance on non-macOS (see the module doc
/// comment above).
pub enum LaunchdOutcome {
    /// Installed (or re-installed) at these paths.
    Installed(LaunchdPaths),
    /// Uninstall result for both managed agents.
    Uninstalled(UninstallResult),
    /// Not macOS — `launchd` has no meaning on this platform.
    NotSupported,
}

/// Dispatch a parsed [`LaunchdSubcommand`]. macOS-gated; every other
/// platform returns [`LaunchdOutcome::NotSupported`] without touching the
/// filesystem.
///
/// # Errors
///
/// Propagates I/O errors from [`install`]/[`uninstall`], and an error if
/// `$HOME` is unset on macOS (there is no sane install target without it).
// The non-macOS branch never returns `Err` — it can't fail, there is
// nothing to do — so `cargo clippy -D warnings` flags the signature as an
// unnecessary `Result` wrap on every non-macOS target. Mirrors the
// `unused_async` allowance on the `doctor macos-*` arms in cmd_doctor.rs.
#[cfg_attr(not(target_os = "macos"), allow(clippy::unnecessary_wraps))]
pub fn run(sub: &LaunchdSubcommand) -> Result<LaunchdOutcome> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("$HOME is not set; cannot resolve ~/Library/LaunchAgents")?;
        match sub {
            LaunchdSubcommand::Install => Ok(LaunchdOutcome::Installed(install(&home)?)),
            LaunchdSubcommand::Uninstall => Ok(LaunchdOutcome::Uninstalled(uninstall(&home)?)),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = sub;
        Ok(LaunchdOutcome::NotSupported)
    }
}

/// The canonical installed paths below `<home>/Library/LaunchAgents`.
#[must_use]
pub fn installed_paths(home: &Path) -> LaunchdPaths {
    let dir = home.join("Library").join("LaunchAgents");
    LaunchdPaths {
        daemon: dir.join(DAEMON_PLIST_FILE_NAME),
        tray: dir.join(TRAY_PLIST_FILE_NAME),
    }
}

/// Install both embedded `LaunchAgent` plists beneath `home`.
///
/// Idempotent (safe to call repeatedly; always yields the same content and
/// mode) and atomic (writes a sibling temp file, `fsync`s it, then
/// `rename`s over the destination — a reader never observes a
/// partially-written plist).
///
/// # Errors
///
/// Propagates I/O errors creating `Library/LaunchAgents`, writing either temp
/// file, setting its permissions, or renaming it into place. A failed tray
/// write is returned even when the daemon write already succeeded; retrying
/// safely replaces the daemon plist before trying the tray again.
pub fn install(home: &Path) -> Result<LaunchdPaths> {
    let paths = installed_paths(home);
    write_agent(&paths.daemon, DAEMON_PLIST_FILE_NAME, EMBEDDED_DAEMON_PLIST)?;
    write_agent(&paths.tray, TRAY_PLIST_FILE_NAME, EMBEDDED_TRAY_PLIST)?;

    Ok(paths)
}

fn write_agent(dest: &Path, file_name: &str, bytes: &[u8]) -> Result<()> {
    let dir = dest.parent().expect("managed path always has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let tmp_path = dir.join(format!(".{file_name}.tmp"));
    {
        let mut tmp = fs::File::create(&tmp_path)
            .with_context(|| format!("creating {}", tmp_path.display()))?;
        tmp.write_all(bytes)
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        tmp.sync_all()
            .with_context(|| format!("syncing {}", tmp_path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644))
            .with_context(|| format!("chmod 0644 {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, dest).with_context(|| format!("installing {}", dest.display()))?;

    Ok(())
}

/// Remove both managed `LaunchAgent` plists under `home`, if present.
///
/// Reports whether each plist was removed. Missing files are idempotent and
/// reported as `false` rather than errors.
///
/// # Errors
///
/// Propagates I/O errors other than "not found".
pub fn uninstall(home: &Path) -> Result<UninstallResult> {
    let paths = installed_paths(home);
    let daemon_removed = remove_agent(&paths.daemon)?;
    let tray_removed = remove_agent(&paths.tray)?;

    Ok(UninstallResult {
        paths,
        daemon_removed,
        tray_removed,
    })
}

fn remove_agent(dest: &Path) -> Result<bool> {
    match fs::remove_file(dest) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", dest.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn install_writes_both_embedded_plists_to_named_paths_mode_0644() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        let installed = install(home).expect("install");

        assert_eq!(installed, installed_paths(home));
        assert_eq!(
            installed.daemon,
            home.join("Library/LaunchAgents/com.legionworks.dormant.plist")
        );
        assert_eq!(
            installed.tray,
            home.join("Library/LaunchAgents/com.legionworks.dormant-tray.plist")
        );

        let daemon_bytes = std::fs::read(&installed.daemon).expect("read installed daemon plist");
        assert_eq!(
            daemon_bytes, EMBEDDED_DAEMON_PLIST,
            "installed daemon bytes must match embedded plist exactly"
        );
        let tray_bytes = std::fs::read(&installed.tray).expect("read installed tray plist");
        assert_eq!(
            tray_bytes, EMBEDDED_TRAY_PLIST,
            "installed tray bytes must match embedded plist exactly"
        );

        for path in [&installed.daemon, &installed.tray] {
            let mode = std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o644, "installed plist must be mode 0644");
        }
    }

    #[test]
    fn install_overwrites_both_agents_idempotently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        let first = install(home).expect("first install");
        std::fs::write(&first.daemon, b"stale daemon").expect("overwrite daemon");
        std::fs::write(&first.tray, b"stale tray").expect("overwrite tray");
        let second = install(home).expect("second install");

        assert_eq!(first, second);
        assert_eq!(
            std::fs::read(&second.daemon).expect("read daemon"),
            EMBEDDED_DAEMON_PLIST
        );
        assert_eq!(
            std::fs::read(&second.tray).expect("read tray"),
            EMBEDDED_TRAY_PLIST
        );
    }

    #[test]
    fn uninstall_removes_both_agents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        install(home).expect("install");

        let result = uninstall(home).expect("uninstall");

        assert!(
            result.daemon_removed,
            "uninstall should remove the daemon plist"
        );
        assert!(
            result.tray_removed,
            "uninstall should remove the tray plist"
        );
        assert!(
            !result.paths.daemon.exists(),
            "installed daemon plist should be gone"
        );
        assert!(
            !result.paths.tray.exists(),
            "installed tray plist should be gone"
        );
    }

    #[test]
    fn uninstall_missing_agents_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        let result = uninstall(home).expect("uninstall of missing files must not error");

        assert!(
            !result.daemon_removed,
            "uninstall of an already-absent daemon plist should report false, not error"
        );
        assert!(
            !result.tray_removed,
            "uninstall of an already-absent tray plist should report false, not error"
        );
    }

    #[test]
    fn uninstall_does_not_touch_sibling_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        install(home).expect("install");

        let sibling = home.join("Library/LaunchAgents/com.other.app.plist");
        std::fs::write(&sibling, b"unrelated").expect("write sibling");

        uninstall(home).expect("uninstall");

        assert!(sibling.exists(), "uninstall must not touch other files");
    }

    #[test]
    fn install_and_uninstall_handle_partial_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let paths = installed_paths(home);
        let parent = paths.daemon.parent().expect("LaunchAgents parent");
        std::fs::create_dir_all(parent).expect("create LaunchAgents directory");
        std::fs::write(&paths.daemon, b"stale daemon").expect("write daemon plist");

        let installed = install(home).expect("install from daemon-only state");
        assert_eq!(
            std::fs::read(&installed.daemon).expect("read daemon"),
            EMBEDDED_DAEMON_PLIST
        );
        assert_eq!(
            std::fs::read(&installed.tray).expect("read tray"),
            EMBEDDED_TRAY_PLIST
        );

        std::fs::remove_file(&installed.tray).expect("remove tray plist");
        let result = uninstall(home).expect("uninstall from daemon-only state");
        assert!(result.daemon_removed, "daemon plist should be removed");
        assert!(
            !result.tray_removed,
            "missing tray plist should not be an error"
        );
    }
}
