//! Shared path-resolution helpers for dormantd and dormantctl.
//!
//! Single implementation of the default-config and default-socket chains so
//! that daemon and CLI agree on where to look.
//!
//! Internal `_from` functions accept explicit env values for testability;
//! public functions read the environment once and delegate.

use std::ffi::OsString;
use std::path::PathBuf;

/// Return the list of candidate config paths, in priority order.
///
/// 1. `$XDG_CONFIG_HOME/dormant/config.toml` (if `XDG_CONFIG_HOME` is set)
/// 2. `$HOME/.config/dormant/config.toml` (if `HOME` is set)
/// 3. `/etc/dormant/config.toml`
#[must_use]
pub fn default_config_candidates() -> Vec<PathBuf> {
    config_candidates_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Internal: build candidate list from explicit env values (test seam).
#[must_use]
fn config_candidates_from(xdg: Option<OsString>, home: Option<OsString>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = xdg {
        candidates.push(PathBuf::from(xdg).join("dormant").join("config.toml"));
    }
    if let Some(home) = home {
        candidates.push(
            PathBuf::from(home)
                .join(".config")
                .join("dormant")
                .join("config.toml"),
        );
    }
    candidates.push(PathBuf::from("/etc/dormant/config.toml"));
    candidates
}

/// Resolve the config path: explicit arg, or the first existing candidate.
///
/// # Errors
///
/// Returns an error string if no candidate exists.
pub fn resolve_config_path(explicit: Option<&std::path::Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    for c in default_config_candidates() {
        if c.exists() {
            return Ok(c);
        }
    }
    Err("no config file found; pass --config or create \
         $XDG_CONFIG_HOME/dormant/config.toml or /etc/dormant/config.toml"
        .into())
}

/// Return the default socket path.
///
/// 1. `$XDG_RUNTIME_DIR/dormant.sock`
/// 2. `/run/dormant/dormant.sock`
#[must_use]
pub fn default_socket_path() -> PathBuf {
    socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Internal: build socket path from explicit env value (test seam).
#[must_use]
fn socket_path_from(runtime_dir: Option<OsString>) -> PathBuf {
    if let Some(dir) = runtime_dir {
        let mut p = PathBuf::from(dir);
        p.push("dormant.sock");
        return p;
    }
    PathBuf::from("/run/dormant/dormant.sock")
}

/// Return the fixed per-user-session lock path.
///
/// The lock path is deliberately NOT config-overridable — unlike the socket,
/// a configurable lock path would defeat the single-instance guard: a second
/// daemon with a different lock path would still start and fight the physical
/// displays.
///
/// 1. `$XDG_RUNTIME_DIR/dormant.lock`
/// 2. `/run/dormant/dormant.lock`
#[must_use]
pub fn default_lock_path() -> PathBuf {
    lock_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Internal: build lock path from explicit env value (test seam).
#[must_use]
fn lock_path_from(runtime_dir: Option<OsString>) -> PathBuf {
    if let Some(dir) = runtime_dir {
        let mut p = PathBuf::from(dir);
        p.push("dormant.lock");
        return p;
    }
    PathBuf::from("/run/dormant/dormant.lock")
}

/// Resolve the socket path from an optional config value or default.
#[must_use]
pub fn resolve_socket_path(config_socket: Option<&std::path::Path>) -> PathBuf {
    config_socket.map_or_else(default_socket_path, std::path::Path::to_path_buf)
}

/// Return the daemon-owned state directory.
///
/// 1. `$XDG_STATE_HOME/dormant` (if `XDG_STATE_HOME` is set)
/// 2. `$HOME/.local/state/dormant` (fallback)
///
/// This is the single implementation of the XDG-state precedence used by
/// any component that persists daemon-owned state (as opposed to
/// `credentials.toml`, which the user owns and edits directly).
#[must_use]
pub fn state_dir() -> PathBuf {
    state_dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// Internal: build the state directory from explicit env values (test seam).
#[must_use]
fn state_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(xdg) = xdg {
        return PathBuf::from(xdg).join("dormant");
    }
    let home = home.unwrap_or_default();
    PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("dormant")
}

/// Public seam onto [`state_dir_from`] for downstream crates that need to
/// derive the state directory from explicit (test-injected) env values
/// rather than reading the process environment directly.
///
/// `state_dir_from` itself stays private — this is a thin, additive
/// pass-through so callers like `dormant-displays` can build a pure
/// "is persistence possible at all" test seam (both env vars absent) on
/// top of a *single* source of XDG-state-vs-`HOME` precedence truth,
/// without duplicating that precedence logic at the call site.
#[must_use]
pub fn state_dir_from_env(xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    state_dir_from(xdg, home)
}

/// Return the `wear` subdirectory of the daemon-owned state directory,
/// where panel-wear tracking data is persisted.
#[must_use]
pub fn wear_state_dir() -> PathBuf {
    state_dir().join("wear")
}

/// `credentials.toml` in the same directory as the config file.
#[must_use]
pub fn sibling_credentials(config_path: &std::path::Path) -> PathBuf {
    config_path.parent().map_or_else(
        || PathBuf::from("credentials.toml"),
        |dir| dir.join("credentials.toml"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_candidates_includes_xdg_when_set() {
        let candidates = config_candidates_from(
            Some(OsString::from("/home/user/xdg")),
            Some(OsString::from("/home/user")),
        );
        assert!(candidates[0].to_string_lossy().contains("/home/user/xdg"));
        assert!(candidates[1].to_string_lossy().contains("/home/user"));
        assert_eq!(candidates[2], PathBuf::from("/etc/dormant/config.toml"));
    }

    #[test]
    fn config_candidates_includes_home_when_xdg_unset() {
        let candidates = config_candidates_from(None, Some(OsString::from("/home/user")));
        assert!(candidates[0].to_string_lossy().contains("/home/user"));
        assert_eq!(candidates[1], PathBuf::from("/etc/dormant/config.toml"));
    }

    #[test]
    fn config_candidates_no_home_no_xdg() {
        let candidates = config_candidates_from(None, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], PathBuf::from("/etc/dormant/config.toml"));
    }

    #[test]
    fn socket_path_from_xdg() {
        let p = socket_path_from(Some(OsString::from("/run/user/1000")));
        assert_eq!(p, PathBuf::from("/run/user/1000/dormant.sock"));
    }

    #[test]
    fn socket_path_from_fallback() {
        let p = socket_path_from(None);
        assert_eq!(p, PathBuf::from("/run/dormant/dormant.sock"));
    }

    #[test]
    fn resolve_socket_path_from_config() {
        let p = resolve_socket_path(Some(std::path::Path::new("/tmp/test.sock")));
        assert_eq!(p, PathBuf::from("/tmp/test.sock"));
    }

    #[test]
    fn sibling_credentials_beside_config() {
        let p = sibling_credentials(std::path::Path::new("/etc/dormant/config.toml"));
        assert_eq!(p, PathBuf::from("/etc/dormant/credentials.toml"));
    }

    #[test]
    fn sibling_credentials_fallback() {
        let p = sibling_credentials(std::path::Path::new("config.toml"));
        assert_eq!(p, PathBuf::from("credentials.toml"));
    }

    #[test]
    fn lock_path_from_xdg() {
        let p = lock_path_from(Some(OsString::from("/run/user/1000")));
        assert_eq!(p, PathBuf::from("/run/user/1000/dormant.lock"));
    }

    #[test]
    fn lock_path_from_fallback() {
        let p = lock_path_from(None);
        assert_eq!(p, PathBuf::from("/run/dormant/dormant.lock"));
    }

    #[test]
    fn state_dir_prefers_xdg_state_home() {
        assert_eq!(
            state_dir_from(Some("/xdg-state".into()), Some("/home/u".into())),
            PathBuf::from("/xdg-state/dormant")
        );
    }

    #[test]
    fn state_dir_falls_back_to_home_local_state() {
        assert_eq!(
            state_dir_from(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.local/state/dormant")
        );
    }

    #[test]
    fn wear_state_dir_is_wear_subdir() {
        assert!(wear_state_dir().ends_with("dormant/wear"));
    }
}
