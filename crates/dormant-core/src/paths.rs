//! Shared path-resolution helpers for dormantd and dormantctl.
//!
//! Single implementation of the default-config and default-socket chains so
//! that daemon and CLI agree on where to look.

use std::path::PathBuf;

/// Return the list of candidate config paths, in priority order.
///
/// 1. `$XDG_CONFIG_HOME/dormant/config.toml` (if `XDG_CONFIG_HOME` is set)
/// 2. `$HOME/.config/dormant/config.toml` (if HOME is set)
/// 3. `/etc/dormant/config.toml`
#[must_use]
pub fn default_config_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(xdg).join("dormant").join("config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
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
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let mut p = PathBuf::from(runtime_dir);
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
    fn default_config_candidates_includes_xdg_when_set() {
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: test-only env manipulation, single-threaded test.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/home/user/xdg");
        }
        let candidates = default_config_candidates();
        assert!(
            candidates
                .iter()
                .any(|p| p.to_string_lossy().contains("/home/user/xdg"))
        );
        match prev {
            Some(v) => unsafe {
                std::env::set_var("XDG_CONFIG_HOME", v);
            },
            None => unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            },
        }
    }

    #[test]
    fn default_config_candidates_includes_home_when_xdg_unset() {
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: test-only env manipulation.
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::set_var("HOME", "/home/user");
        }
        let candidates = default_config_candidates();
        assert!(
            candidates
                .iter()
                .any(|p| p.to_string_lossy().contains("/home/user"))
        );
        match prev_xdg {
            Some(v) => unsafe {
                std::env::set_var("XDG_CONFIG_HOME", v);
            },
            None => unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            },
        }
        match prev_home {
            Some(v) => unsafe {
                std::env::set_var("HOME", v);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
    }

    #[test]
    fn xdg_set_but_missing_falls_back_to_home() {
        // When XDG_CONFIG_HOME is set but the file doesn't exist,
        // resolve_config_path should still try HOME and /etc.
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: test-only env manipulation.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/nonexistent-xdg");
            std::env::set_var("HOME", "/nonexistent-home");
        }
        // Neither exists, so it should error
        let result = resolve_config_path(None);
        assert!(result.is_err());
        match prev_xdg {
            Some(v) => unsafe {
                std::env::set_var("XDG_CONFIG_HOME", v);
            },
            None => unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            },
        }
        match prev_home {
            Some(v) => unsafe {
                std::env::set_var("HOME", v);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
    }

    #[test]
    fn default_socket_path_xdg() {
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: test-only env manipulation.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        let p = default_socket_path();
        assert_eq!(p, PathBuf::from("/run/user/1000/dormant.sock"));
        match prev {
            Some(v) => unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", v);
            },
            None => unsafe {
                std::env::remove_var("XDG_RUNTIME_DIR");
            },
        }
    }

    #[test]
    fn default_socket_path_fallback() {
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: test-only env manipulation.
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let p = default_socket_path();
        assert_eq!(p, PathBuf::from("/run/dormant/dormant.sock"));
        match prev {
            Some(v) => unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", v);
            },
            None => unsafe {
                std::env::remove_var("XDG_RUNTIME_DIR");
            },
        }
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
