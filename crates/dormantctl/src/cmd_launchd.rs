//! `dormantctl launchd` — install/uninstall the checked-in macOS
//! `LaunchAgent` plist.
//!
//! `install`/`uninstall`/`installed_path` are NOT `cfg(target_os =
//! "macos")`-gated: they take an explicit `home` directory and touch only
//! the filesystem beneath it, so they are fully exercisable on Linux (see
//! the tests below). Only [`run`] — the CLI-facing entry point dispatched
//! from `main.rs` — is platform-gated, mirroring the pre-existing
//! `Ddcci`/`Kwin`/`macos-*` doctor-arm pattern in `cmd_doctor.rs`: parsing
//! of `launchd install`/`launchd uninstall` is unconditional (works on
//! every platform's `--help`), only the handler behind it differs.
//!
//! Because `install`/`uninstall`/`EMBEDDED_PLIST`/etc. are otherwise only
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

/// The launchd agent plist, embedded at build time from the SAME checked-in
/// file cargo-dist stages into every macOS release archive at
/// `share/com.legionworks.dormant.plist` (see
/// `crates/dormantd/Cargo.toml`'s package-local `[package.metadata.dist]
/// include`). A single `include_bytes!` of that one file is what makes "the
/// archived copy and the installed copy are identical" true by
/// construction rather than by convention that could silently drift.
pub const EMBEDDED_PLIST: &[u8] =
    include_bytes!("../../dormantd/share/com.legionworks.dormant.plist");

const PLIST_FILE_NAME: &str = "com.legionworks.dormant.plist";

/// CLI surface for `dormantctl launchd <install|uninstall>`.
#[derive(clap::Subcommand, Debug)]
pub enum LaunchdSubcommand {
    /// Install the checked-in `LaunchAgent` plist to
    /// `~/Library/LaunchAgents/com.legionworks.dormant.plist`.
    ///
    /// Explicit, non-root, idempotent: re-running overwrites the file with
    /// the same embedded bytes (atomically — a concurrent reader/launchctl
    /// never observes a partially-written plist) rather than erroring.
    /// Does NOT bootstrap or start the agent; run
    /// `launchctl bootstrap gui/$UID
    /// "$HOME/Library/LaunchAgents/com.legionworks.dormant.plist"`
    /// yourself afterward.
    Install,
    /// Remove the canonical installed plist, if present.
    ///
    /// Only ever removes that ONE file. Does not bootout the running
    /// launchd label — run `launchctl bootout gui/$UID
    /// com.legionworks.dormant` first if the agent is loaded, otherwise
    /// launchd keeps the last-loaded definition in memory even after the
    /// on-disk file is gone.
    Uninstall,
}

/// Outcome of [`run`], for `main.rs` to render and exit-code accordingly.
///
/// `Installed`/`Uninstalled` are only ever constructed from the
/// `cfg(target_os = "macos")` half of [`run`]; covered by this file's
/// module-wide `dead_code` allowance on non-macOS (see the module doc
/// comment above).
pub enum LaunchdOutcome {
    /// Installed (or re-installed) at this path.
    Installed(PathBuf),
    /// Uninstall attempted at this path; `removed` is `false` when the
    /// file was already absent (idempotent, not an error).
    Uninstalled { path: PathBuf, removed: bool },
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
            LaunchdSubcommand::Uninstall => {
                let path = installed_path(&home);
                let removed = uninstall(&home)?;
                Ok(LaunchdOutcome::Uninstalled { path, removed })
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = sub;
        Ok(LaunchdOutcome::NotSupported)
    }
}

/// The canonical installed path: `<home>/Library/LaunchAgents/com.legionworks.dormant.plist`.
#[must_use]
pub fn installed_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILE_NAME)
}

/// Install [`EMBEDDED_PLIST`] to [`installed_path`] under `home`.
///
/// Idempotent (safe to call repeatedly; always yields the same content and
/// mode) and atomic (writes a sibling temp file, `fsync`s it, then
/// `rename`s over the destination — a reader never observes a
/// partially-written plist).
///
/// # Errors
///
/// Propagates I/O errors creating `Library/LaunchAgents`, writing the temp
/// file, setting its permissions, or renaming it into place.
pub fn install(home: &Path) -> Result<PathBuf> {
    let dest = installed_path(home);
    let dir = dest
        .parent()
        .expect("installed_path always yields a path with a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let tmp_path = dir.join(format!(".{PLIST_FILE_NAME}.tmp"));
    {
        let mut tmp = fs::File::create(&tmp_path)
            .with_context(|| format!("creating {}", tmp_path.display()))?;
        tmp.write_all(EMBEDDED_PLIST)
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
    fs::rename(&tmp_path, &dest).with_context(|| format!("installing {}", dest.display()))?;

    Ok(dest)
}

/// Remove the canonical installed plist under `home`, if present.
///
/// Returns `Ok(true)` if a file was removed, `Ok(false)` if it was already
/// absent (idempotent — not an error).
///
/// # Errors
///
/// Propagates I/O errors other than "not found".
pub fn uninstall(home: &Path) -> Result<bool> {
    let dest = installed_path(home);
    match fs::remove_file(&dest) {
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
    fn install_writes_embedded_bytes_to_canonical_path_mode_0644() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        let installed = install(home).expect("install");

        assert_eq!(installed, installed_path(home));
        assert_eq!(
            installed,
            home.join("Library/LaunchAgents/com.legionworks.dormant.plist")
        );

        let bytes = std::fs::read(&installed).expect("read installed plist");
        assert_eq!(
            bytes, EMBEDDED_PLIST,
            "installed bytes must match embedded plist exactly"
        );

        let mode = std::fs::metadata(&installed)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "installed plist must be mode 0644");
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        let first = install(home).expect("first install");
        let second = install(home).expect("second install");

        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second).expect("read"), EMBEDDED_PLIST);
    }

    #[test]
    fn uninstall_removes_canonical_file_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        install(home).expect("install");

        let removed = uninstall(home).expect("uninstall");

        assert!(removed, "uninstall should report it removed the file");
        assert!(
            !installed_path(home).exists(),
            "installed plist should be gone"
        );
    }

    #[test]
    fn uninstall_missing_file_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();

        let removed = uninstall(home).expect("uninstall of missing file must not error");

        assert!(
            !removed,
            "uninstall of an already-absent file should report false, not error"
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
}
