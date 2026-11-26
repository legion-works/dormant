//! Commands for the controller's `sh -c` and `cmd /C` shells.

/// Succeed without output: `true` on Unix and `exit 0` on Windows.
pub fn ok() -> &'static str {
    if cfg!(windows) { "exit 0" } else { "true" }
}
