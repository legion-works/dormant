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

/// Resolve the socket path from an optional config value or default.
#[must_use]
pub fn resolve_socket_path(config_socket: Option<&std::path::Path>) -> PathBuf {
    config_socket.map_or_else(default_socket_path, std::path::Path::to_path_buf)
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
}
