//! Shared path-resolution helpers for dormantd and dormantctl.
//!
//! Single implementation of the default-config and default-socket chains so
//! that daemon and CLI agree on where to look.
//!
//! Internal `_from` functions accept explicit env values for testability;
//! public functions read the environment once and delegate.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Compile-time target routing for platform-specific default-path derivation.
///
/// The `_with` functions below take this as an explicit parameter (rather
/// than reading `cfg!(target_os = ...)` internally) so that the Linux, macOS,
/// and Windows routes are all exercised by tests on any host — including this
/// Linux CI/dev sandbox, which can never compile/run Windows- or macOS-target
/// code but CAN exercise each target's path-derivation *logic* via `TargetOs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    /// XDG-style paths: `$XDG_RUNTIME_DIR`, `$HOME/.config`, `/run/dormant`.
    Linux,
    /// Apple-style paths: `~/Library/Application Support`, state-dir-derived
    /// socket/lock. Deliberately never reads `$XDG_RUNTIME_DIR` or `$TMPDIR`.
    Macos,
    /// Windows-style paths: `%APPDATA%`/`%PROGRAMDATA%` config, `%LOCALAPPDATA%`
    /// state/lock, and a `\\.\pipe\` named pipe for the socket. Deliberately
    /// never reads `$XDG_RUNTIME_DIR`, `$HOME`, or `/run/dormant`.
    Windows,
}

impl TargetOs {
    /// The target this binary was actually compiled for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }
}

/// Environment values consulted by path derivation, injected explicitly so a
/// test on any host can exercise every target's logic.
///
/// A struct rather than a widening list of positional `Option<OsString>`
/// parameters: Windows alone adds four values (`APPDATA`, `LOCALAPPDATA`,
/// `PROGRAMDATA`, `USERNAME`), which would push the socket/lock seams to seven
/// positional arguments — swap-prone and unreadable at the call site.
#[derive(Debug, Clone, Default)]
pub struct PathEnv {
    /// `$XDG_CONFIG_HOME` — explicit XDG override, honoured on every target.
    pub xdg_config_home: Option<OsString>,
    /// `$HOME` — Linux/macOS home directory.
    pub home: Option<OsString>,
    /// `$XDG_RUNTIME_DIR` — Linux-only runtime dir; never read on macOS/Windows.
    pub xdg_runtime_dir: Option<OsString>,
    /// `$XDG_STATE_HOME` — Linux/macOS state-dir override.
    pub xdg_state_home: Option<OsString>,
    /// `%APPDATA%` — Windows roaming application data.
    pub appdata: Option<OsString>,
    /// `%LOCALAPPDATA%` — Windows local application data.
    pub localappdata: Option<OsString>,
    /// `%PROGRAMDATA%` — Windows machine-wide application data.
    pub programdata: Option<OsString>,
    /// `%USERPROFILE%` — Windows user profile directory.
    pub userprofile: Option<OsString>,
    /// `%USERNAME%` — Windows account name, used in the named-pipe name.
    pub username: Option<OsString>,
}

impl PathEnv {
    /// Read every consulted environment variable once.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            home: std::env::var_os("HOME"),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
            xdg_state_home: std::env::var_os("XDG_STATE_HOME"),
            appdata: std::env::var_os("APPDATA"),
            localappdata: std::env::var_os("LOCALAPPDATA"),
            programdata: std::env::var_os("PROGRAMDATA"),
            userprofile: std::env::var_os("USERPROFILE"),
            username: std::env::var_os("USERNAME"),
        }
    }
}

/// Return the list of candidate config paths, in priority order.
///
/// Linux:
/// 1. `$XDG_CONFIG_HOME/dormant/config.toml` (if `XDG_CONFIG_HOME` is set)
/// 2. `$HOME/.config/dormant/config.toml` (if `HOME` is set)
/// 3. `/etc/dormant/config.toml`
///
/// macOS:
/// 1. `$XDG_CONFIG_HOME/dormant/config.toml` (if `XDG_CONFIG_HOME` is set —
///    explicit XDG overrides keep working even on macOS)
/// 2. `$HOME/Library/Application Support/dormant/config.toml` (if `HOME` is set)
/// 3. `/etc/dormant/config.toml`
///
/// Windows:
/// 1. `%XDG_CONFIG_HOME%\dormant\config.toml` (if `XDG_CONFIG_HOME` is set —
///    explicit XDG overrides keep working even on Windows)
/// 2. `%APPDATA%\dormant\config.toml` (if `APPDATA` is set)
/// 3. `%PROGRAMDATA%\dormant\config.toml` (if `PROGRAMDATA` is set)
#[must_use]
pub fn default_config_candidates() -> Vec<PathBuf> {
    default_config_candidates_with(TargetOs::current(), &PathEnv::from_process())
}

/// Target-routed, env-injected config-candidate derivation (test seam).
#[must_use]
pub fn default_config_candidates_with(target: TargetOs, env: &PathEnv) -> Vec<PathBuf> {
    match target {
        TargetOs::Linux => config_candidates_from(env.xdg_config_home.clone(), env.home.clone()),
        TargetOs::Macos => {
            macos_config_candidates_from(env.xdg_config_home.clone(), env.home.clone())
        }
        TargetOs::Windows => windows_config_candidates_from(env),
    }
}

/// Internal: build the Linux candidate list from explicit env values (test seam).
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

/// Internal: build the macOS candidate list from explicit env values (test
/// seam). `$TMPDIR` is never consulted — Apple apps commonly get a
/// per-session-unique `$TMPDIR`, which would silently fragment config
/// discovery across sessions if it leaked in here.
#[must_use]
fn macos_config_candidates_from(xdg: Option<OsString>, home: Option<OsString>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = xdg {
        candidates.push(PathBuf::from(xdg).join("dormant").join("config.toml"));
    }
    if let Some(home) = home {
        candidates.push(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("dormant")
                .join("config.toml"),
        );
    }
    candidates.push(PathBuf::from("/etc/dormant/config.toml"));
    candidates
}

/// Internal: build the Windows candidate list from explicit env values (test
/// seam). `$HOME` and `/etc/dormant` are never consulted — Windows has no
/// `$HOME` and no `/etc`, so falling back to either would resolve a path the
/// OS cannot use. Each candidate is included only when its env var is set.
#[must_use]
fn windows_config_candidates_from(env: &PathEnv) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = &env.xdg_config_home {
        candidates.push(PathBuf::from(xdg).join("dormant").join("config.toml"));
    }
    if let Some(appdata) = &env.appdata {
        candidates.push(PathBuf::from(appdata).join("dormant").join("config.toml"));
    }
    if let Some(programdata) = &env.programdata {
        candidates.push(
            PathBuf::from(programdata)
                .join("dormant")
                .join("config.toml"),
        );
    }
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
/// Linux:
/// 1. `$XDG_RUNTIME_DIR/dormant.sock`
/// 2. `/run/dormant/dormant.sock`
///
/// macOS: derived from [`state_dir`] (`$XDG_STATE_HOME/dormant`, falling
/// back to `~/.local/state/dormant`) — `$XDG_RUNTIME_DIR` is deliberately
/// IGNORED on this route (session-scoped on Linux via systemd/logind; macOS
/// has no equivalent per-login-session runtime dir with the same lifetime
/// guarantees), and `$TMPDIR` is never read at all.
///
/// Windows: a named pipe, `\\.\pipe\dormant-<username>` (carried as a
/// [`PathBuf`] like the unix socket path). `$XDG_RUNTIME_DIR`, `$HOME`, and
/// `/run/dormant` are never consulted.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    default_socket_path_with(TargetOs::current(), &PathEnv::from_process())
}

/// Target-routed, env-injected socket-path derivation (test seam).
#[must_use]
pub fn default_socket_path_with(target: TargetOs, env: &PathEnv) -> PathBuf {
    match target {
        TargetOs::Linux => socket_path_from(env.xdg_runtime_dir.clone()),
        TargetOs::Macos => {
            state_dir_from(env.xdg_state_home.clone(), env.home.clone()).join("dormant.sock")
        }
        TargetOs::Windows => windows_socket_path_from(env),
    }
}

/// Internal: build the Linux socket path from explicit env value (test seam).
#[must_use]
fn socket_path_from(runtime_dir: Option<OsString>) -> PathBuf {
    if let Some(dir) = runtime_dir {
        let mut p = PathBuf::from(dir);
        p.push("dormant.sock");
        return p;
    }
    PathBuf::from("/run/dormant/dormant.sock")
}

/// Internal: build the Windows named-pipe path from explicit env values (test
/// seam). The pipe name is `\\.\pipe\dormant-<username>`; `%USERNAME%` is
/// sanitized so the name is always well-formed.
#[must_use]
fn windows_socket_path_from(env: &PathEnv) -> PathBuf {
    let user = sanitize_pipe_component(env.username.as_deref());
    PathBuf::from(format!(r"\\.\pipe\dormant-{user}"))
}

/// Sanitize a Windows account name into a named-pipe name component.
///
/// A pipe name cannot contain a backslash (it terminates the name), so any
/// backslash — and, defensively, any forward slash or control character — is
/// replaced with `_`. An absent or empty name falls back to `default` so the
/// pipe is never the malformed `dormant-`.
#[must_use]
fn sanitize_pipe_component(raw: Option<&OsStr>) -> String {
    let Some(raw) = raw else {
        return "default".to_string();
    };
    let sanitized: String = raw
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c == '\\' || c == '/' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

/// Return the fixed per-user-session lock path.
///
/// The lock path is deliberately NOT config-overridable — unlike the socket,
/// a configurable lock path would defeat the single-instance guard: a second
/// daemon with a different lock path would still start and fight the physical
/// displays.
///
/// Linux:
/// 1. `$XDG_RUNTIME_DIR/dormant.lock`
/// 2. `/run/dormant/dormant.lock`
///
/// macOS: derived from [`state_dir`], same routing rationale as
/// [`default_socket_path`] — `$XDG_RUNTIME_DIR` and `$TMPDIR` are ignored.
///
/// Windows: `%LOCALAPPDATA%\dormant\dormant.lock` (same filename as Linux —
/// one lock identity across platforms). `$XDG_RUNTIME_DIR`, `$HOME`, and
/// `/run/dormant` are never consulted.
#[must_use]
pub fn default_lock_path() -> PathBuf {
    default_lock_path_with(TargetOs::current(), &PathEnv::from_process())
}

/// Target-routed, env-injected lock-path derivation (test seam).
#[must_use]
pub fn default_lock_path_with(target: TargetOs, env: &PathEnv) -> PathBuf {
    match target {
        TargetOs::Linux => lock_path_from(env.xdg_runtime_dir.clone()),
        // Same filename as Linux (`dormant.lock`): one lock identity across platforms.
        TargetOs::Macos => {
            state_dir_from(env.xdg_state_home.clone(), env.home.clone()).join("dormant.lock")
        }
        TargetOs::Windows => windows_lock_path_from(env),
    }
}

/// Internal: build the Linux lock path from explicit env value (test seam).
#[must_use]
fn lock_path_from(runtime_dir: Option<OsString>) -> PathBuf {
    if let Some(dir) = runtime_dir {
        let mut p = PathBuf::from(dir);
        p.push("dormant.lock");
        return p;
    }
    PathBuf::from("/run/dormant/dormant.lock")
}

/// Internal: build the Windows lock path from explicit env values (test seam).
#[must_use]
fn windows_lock_path_from(env: &PathEnv) -> PathBuf {
    windows_local_appdata_dir(env)
        .join("dormant")
        .join("dormant.lock")
}

/// The Windows per-user local application-data directory, with the fallback
/// chain `%LOCALAPPDATA%` → `%USERPROFILE%\AppData\Local` → `C:\ProgramData`.
///
/// The final fallback is an absolute, machine-wide location so the derived
/// path is never relative (a bare `dormant\...` would resolve against the
/// process working directory and fragment state across launches).
#[must_use]
fn windows_local_appdata_dir(env: &PathEnv) -> PathBuf {
    if let Some(local) = &env.localappdata {
        return PathBuf::from(local);
    }
    if let Some(profile) = &env.userprofile {
        return PathBuf::from(profile).join("AppData").join("Local");
    }
    PathBuf::from(r"C:\ProgramData")
}

/// Resolve the socket path from an optional config value or default.
#[must_use]
pub fn resolve_socket_path(config_socket: Option<&std::path::Path>) -> PathBuf {
    config_socket.map_or_else(default_socket_path, std::path::Path::to_path_buf)
}

/// Return the daemon-owned state directory.
///
/// Linux/macOS:
/// 1. `$XDG_STATE_HOME/dormant` (if `XDG_STATE_HOME` is set)
/// 2. `$HOME/.local/state/dormant` (fallback)
///
/// Windows: `%LOCALAPPDATA%\dormant\state`, falling back to
/// `%USERPROFILE%\AppData\Local\dormant\state` and then
/// `C:\ProgramData\dormant\state`.
///
/// This is the single implementation of the state-directory precedence used
/// by any component that persists daemon-owned state (as opposed to
/// `credentials.toml`, which the user owns and edits directly).
#[must_use]
pub fn state_dir() -> PathBuf {
    state_dir_with(TargetOs::current(), &PathEnv::from_process())
}

/// Target-routed, env-injected state-directory derivation (test seam).
#[must_use]
pub fn state_dir_with(target: TargetOs, env: &PathEnv) -> PathBuf {
    match target {
        TargetOs::Linux | TargetOs::Macos => {
            state_dir_from(env.xdg_state_home.clone(), env.home.clone())
        }
        TargetOs::Windows => windows_state_dir_from(env),
    }
}

/// Internal: build the Linux/macOS state directory from explicit env values
/// (test seam).
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

/// Internal: build the Windows state directory from explicit env values (test
/// seam).
#[must_use]
fn windows_state_dir_from(env: &PathEnv) -> PathBuf {
    windows_local_appdata_dir(env).join("dormant").join("state")
}

/// Public seam onto `state_dir_from` for downstream crates that need to
/// derive the state directory from explicit (test-injected) env values
/// rather than reading the process environment directly.
///
/// `state_dir_from` itself stays private — this is a thin, additive
/// pass-through so callers like `dormant-displays` can build a pure
/// "is persistence possible at all" test seam (both env vars absent) on
/// top of a *single* source of XDG-state-vs-`HOME` precedence truth,
/// without duplicating that precedence logic at the call site.
///
/// This is the XDG (Linux/macOS) derivation specifically; the Windows route
/// lives in [`state_dir_with`].
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
        // `ends_with("dormant/wear")` encoded the Linux layout, where
        // `state_dir()` is `$XDG_STATE_HOME/dormant`. Windows derives
        // `%LOCALAPPDATA%\dormant\state`, so the last two components are
        // `state` and `wear` and the old assertion failed there for the wrong
        // reason. Assert the invariant the test is named for instead.
        let state = state_dir();
        let wear = wear_state_dir();
        assert!(
            wear.starts_with(&state),
            "wear dir {wear:?} must live under the state dir {state:?}"
        );
        assert_eq!(
            wear.file_name().and_then(std::ffi::OsStr::to_str),
            Some("wear"),
            "wear dir must be the `wear` subdirectory"
        );
        assert!(
            state
                .components()
                .any(|c| c.as_os_str() == std::ffi::OsStr::new("dormant")),
            "state dir {state:?} must be dormant-scoped"
        );
    }

    // ── Task 5: macOS path routing ──────────────────────────────────────────

    #[test]
    fn macos_defaults_keep_socket_and_lock_out_of_tmpdir() {
        // TMPDIR is deliberately never a parameter anywhere in this module —
        // it structurally cannot leak into any of these derivations. We still
        // name it here to document the scenario the test guards against.
        let home = Some(OsString::from("/Users/alice"));
        let xdg_runtime_dir = Some(OsString::from("/var/run/session-a"));

        let candidates = default_config_candidates_with(
            TargetOs::Macos,
            &PathEnv {
                home: home.clone(),
                ..Default::default()
            },
        );
        assert_eq!(
            candidates[0],
            PathBuf::from("/Users/alice/Library/Application Support/dormant/config.toml")
        );

        let state = state_dir_from(None, home.clone());
        assert_eq!(state, PathBuf::from("/Users/alice/.local/state/dormant"));

        let socket = default_socket_path_with(
            TargetOs::Macos,
            &PathEnv {
                home: home.clone(),
                xdg_runtime_dir: xdg_runtime_dir.clone(),
                ..Default::default()
            },
        );
        assert_eq!(
            socket,
            PathBuf::from("/Users/alice/.local/state/dormant/dormant.sock")
        );

        let lock = default_lock_path_with(
            TargetOs::Macos,
            &PathEnv {
                home,
                xdg_runtime_dir,
                ..Default::default()
            },
        );
        assert_eq!(
            lock,
            PathBuf::from("/Users/alice/.local/state/dormant/dormant.lock")
        );
    }

    #[test]
    fn linux_socket_and_lock_remain_on_xdg_runtime_dir() {
        // Regression guard: the macOS routing addition must not perturb the
        // Linux route at all — same byte-for-byte output as before Task 5.
        let xdg_runtime_dir = Some(OsString::from("/run/user/1000"));
        let xdg_state_home = Some(OsString::from("/home/alice/.state"));
        let home = Some(OsString::from("/home/alice"));

        let socket = default_socket_path_with(
            TargetOs::Linux,
            &PathEnv {
                home: home.clone(),
                xdg_runtime_dir: xdg_runtime_dir.clone(),
                xdg_state_home: xdg_state_home.clone(),
                ..Default::default()
            },
        );
        assert_eq!(socket, PathBuf::from("/run/user/1000/dormant.sock"));

        let lock = default_lock_path_with(
            TargetOs::Linux,
            &PathEnv {
                home,
                xdg_runtime_dir,
                xdg_state_home,
                ..Default::default()
            },
        );
        assert_eq!(lock, PathBuf::from("/run/user/1000/dormant.lock"));
    }

    #[test]
    fn tmpdir_does_not_change_the_resolved_socket() {
        // Two "envs" that would differ only in $TMPDIR collapse to the same
        // call because none of the `_with` signatures accept a TMPDIR
        // parameter at all — TMPDIR cannot reach path derivation by
        // construction, not merely by convention.
        let home = Some(OsString::from("/Users/alice"));
        let xdg_runtime_dir = Some(OsString::from("/var/run/session-a"));

        let a = default_socket_path_with(
            TargetOs::Macos,
            &PathEnv {
                home: home.clone(),
                xdg_runtime_dir: xdg_runtime_dir.clone(),
                ..Default::default()
            },
        );
        let b = default_socket_path_with(
            TargetOs::Macos,
            &PathEnv {
                home,
                xdg_runtime_dir,
                ..Default::default()
            },
        );
        assert_eq!(a, b);
        assert_eq!(
            a,
            PathBuf::from("/Users/alice/.local/state/dormant/dormant.sock")
        );
    }

    // ── Task: Windows path routing ──────────────────────────────────────────

    #[test]
    fn windows_config_candidates_priority_order() {
        let env = PathEnv {
            xdg_config_home: Some("/xdg".into()),
            appdata: Some(r"C:\Users\alice\AppData\Roaming".into()),
            programdata: Some(r"C:\ProgramData".into()),
            ..Default::default()
        };
        let candidates = default_config_candidates_with(TargetOs::Windows, &env);
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates[0],
            PathBuf::from("/xdg").join("dormant").join("config.toml")
        );
        assert_eq!(
            candidates[1],
            PathBuf::from(r"C:\Users\alice\AppData\Roaming")
                .join("dormant")
                .join("config.toml")
        );
        assert_eq!(
            candidates[2],
            PathBuf::from(r"C:\ProgramData")
                .join("dormant")
                .join("config.toml")
        );
    }

    #[test]
    fn windows_config_candidates_without_xdg() {
        let env = PathEnv {
            appdata: Some(r"C:\Users\alice\AppData\Roaming".into()),
            programdata: Some(r"C:\ProgramData".into()),
            ..Default::default()
        };
        let candidates = default_config_candidates_with(TargetOs::Windows, &env);
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0],
            PathBuf::from(r"C:\Users\alice\AppData\Roaming")
                .join("dormant")
                .join("config.toml")
        );
        assert_eq!(
            candidates[1],
            PathBuf::from(r"C:\ProgramData")
                .join("dormant")
                .join("config.toml")
        );
    }

    #[test]
    fn windows_state_dir_from_localappdata() {
        let env = PathEnv {
            localappdata: Some(r"C:\Users\alice\AppData\Local".into()),
            ..Default::default()
        };
        assert_eq!(
            state_dir_with(TargetOs::Windows, &env),
            PathBuf::from(r"C:\Users\alice\AppData\Local")
                .join("dormant")
                .join("state")
        );
    }

    #[test]
    fn windows_state_dir_falls_back_to_userprofile() {
        let env = PathEnv {
            userprofile: Some(r"C:\Users\alice".into()),
            ..Default::default()
        };
        assert_eq!(
            state_dir_with(TargetOs::Windows, &env),
            PathBuf::from(r"C:\Users\alice")
                .join("AppData")
                .join("Local")
                .join("dormant")
                .join("state")
        );
    }

    #[test]
    fn windows_state_dir_final_fallback_is_absolute() {
        // Neither LOCALAPPDATA nor USERPROFILE set: the last resort must be an
        // absolute, machine-wide path, never a bare relative `dormant\state`.
        let dir = state_dir_with(TargetOs::Windows, &PathEnv::default());
        assert_eq!(
            dir,
            PathBuf::from(r"C:\ProgramData")
                .join("dormant")
                .join("state")
        );
        assert!(
            dir.to_string_lossy().starts_with(r"C:\"),
            "final fallback must be rooted at C:\\, got {dir:?}"
        );
    }

    #[test]
    fn windows_socket_path_is_named_pipe() {
        let env = PathEnv {
            username: Some("alice".into()),
            ..Default::default()
        };
        assert_eq!(
            default_socket_path_with(TargetOs::Windows, &env),
            PathBuf::from(r"\\.\pipe\dormant-alice")
        );
    }

    #[test]
    fn windows_pipe_name_sanitizes_backslash() {
        // A pipe-name component cannot contain a backslash — it terminates the
        // name. A domain-qualified account name must not leak one through.
        let env = PathEnv {
            username: Some(r"DOMAIN\alice".into()),
            ..Default::default()
        };
        let p = default_socket_path_with(TargetOs::Windows, &env);
        assert_eq!(p, PathBuf::from(r"\\.\pipe\dormant-DOMAIN_alice"));
        assert!(
            !p.to_string_lossy().contains(r"DOMAIN\alice"),
            "raw backslash leaked into pipe name: {p:?}"
        );
    }

    #[test]
    fn windows_pipe_name_empty_username_falls_back() {
        let env = PathEnv {
            username: Some("".into()),
            ..Default::default()
        };
        assert_eq!(
            default_socket_path_with(TargetOs::Windows, &env),
            PathBuf::from(r"\\.\pipe\dormant-default")
        );
    }

    #[test]
    fn windows_pipe_name_unset_username_falls_back() {
        let p = default_socket_path_with(TargetOs::Windows, &PathEnv::default());
        assert_eq!(p, PathBuf::from(r"\\.\pipe\dormant-default"));
        assert!(
            !p.to_string_lossy().ends_with("dormant-"),
            "unset username produced a trailing-dash pipe name: {p:?}"
        );
    }

    #[test]
    fn windows_lock_path_from_localappdata() {
        let env = PathEnv {
            localappdata: Some(r"C:\Users\alice\AppData\Local".into()),
            ..Default::default()
        };
        assert_eq!(
            default_lock_path_with(TargetOs::Windows, &env),
            PathBuf::from(r"C:\Users\alice\AppData\Local")
                .join("dormant")
                .join("dormant.lock")
        );
    }

    #[test]
    fn windows_defaults_keep_socket_and_lock_out_of_xdg_and_home() {
        // XDG_RUNTIME_DIR and HOME are set to obvious sentinels; the Windows
        // route must consult neither. This pins the "never XDG on Windows"
        // rule — a Windows arm that fell through to the Linux arm would leak
        // the runtime-dir sentinel into the socket path and fail here.
        let env = PathEnv {
            home: Some("/home/SENTINEL-HOME".into()),
            xdg_runtime_dir: Some("/run/user/1000/SENTINEL-RUNTIME".into()),
            localappdata: Some(r"C:\Users\alice\AppData\Local".into()),
            username: Some("alice".into()),
            ..Default::default()
        };

        let socket = default_socket_path_with(TargetOs::Windows, &env);
        let lock = default_lock_path_with(TargetOs::Windows, &env);
        let socket_s = socket.to_string_lossy();
        let lock_s = lock.to_string_lossy();

        assert!(
            !socket_s.contains("SENTINEL-RUNTIME"),
            "socket leaked XDG_RUNTIME_DIR: {socket_s}"
        );
        assert!(
            !socket_s.contains("SENTINEL-HOME"),
            "socket leaked HOME: {socket_s}"
        );
        assert!(
            !lock_s.contains("SENTINEL-RUNTIME"),
            "lock leaked XDG_RUNTIME_DIR: {lock_s}"
        );
        assert!(
            !lock_s.contains("SENTINEL-HOME"),
            "lock leaked HOME: {lock_s}"
        );
        assert!(
            !socket_s.contains("/run/dormant"),
            "socket fell back to /run/dormant: {socket_s}"
        );
        assert!(
            !lock_s.contains("/run/dormant"),
            "lock fell back to /run/dormant: {lock_s}"
        );

        assert_eq!(socket, PathBuf::from(r"\\.\pipe\dormant-alice"));
        assert_eq!(
            lock,
            PathBuf::from(r"C:\Users\alice\AppData\Local")
                .join("dormant")
                .join("dormant.lock")
        );
    }

    #[cfg(windows)]
    #[test]
    fn current_target_is_windows() {
        assert_eq!(TargetOs::current(), TargetOs::Windows);
    }
}
