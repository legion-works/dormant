//! Commands for the controller's `sh -c` and `cmd /C` shells.

use std::path::Path;

/// Succeed without output: `true` on Unix and `exit 0` on Windows.
pub fn ok() -> &'static str {
    if cfg!(windows) { "exit 0" } else { "true" }
}

/// Fail without output: `false` on Unix and `exit 1` on Windows.
pub fn fail() -> &'static str {
    if cfg!(windows) { "exit 1" } else { "false" }
}

/// Append letters or digits without a newline: `printf` on Unix and `set /p` on Windows.
pub fn append(path: &Path, text: &str) -> String {
    assert!(
        !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "test marker text must contain only ASCII letters or digits"
    );
    if cfg!(windows) {
        let path = cmd_path(path);
        // set /p prints the prompt but gets EOF from NUL. Apply the output
        // redirection to the group so a failed open cannot reach exit 0.
        format!("(<nul set /p ={text}& exit 0)>>{path}")
    } else {
        let path = sh_path(path);
        format!("printf %s '{text}' >> {path}")
    }
}

fn cmd_path(path: &Path) -> &str {
    let path = path.to_str().expect("UTF-8 test marker path");
    // cmd does not recognize the argument escapes that Rust adds to quotes.
    assert!(
        !path.contains(|ch: char| ch.is_whitespace() || "\"&|<>()^%!".contains(ch)),
        "Windows test marker path must have no spaces or cmd metacharacters: {path}"
    );
    path
}

fn sh_path(path: &Path) -> String {
    let path = path.to_str().expect("UTF-8 test marker path");
    format!("'{}'", path.replace('\'', "'\\''"))
}

// Keep the loop in cmd so killing the shell also stops the loop.
const CMD_LOOP: &str = "for /l %i in (1,1,2147483647) do";

fn cmd_hold() -> String {
    format!("{CMD_LOOP} @set DORMANT_TEST_HOLD=1")
}

/// Hold until the controller cancels the command or its timeout expires.
pub fn hold(path: &Path) -> String {
    if cfg!(windows) {
        cmd_hold()
    } else {
        format!("exec tail -f {}", sh_path(path))
    }
}

/// Write the start marker before holding the command open.
pub fn append_start_then_hold(marker: &Path, hold_path: &Path) -> String {
    if cfg!(windows) {
        format!("<nul set /p =START>>{}& {}", cmd_path(marker), cmd_hold())
    } else {
        format!("{}; {}", append(marker, "START"), hold(hold_path))
    }
}

/// Write the marker only after the release file exists.
pub fn append_after_release(marker: &Path, text: &str, release: &Path) -> String {
    if cfg!(windows) {
        let release = cmd_path(release);
        let append = append(marker, text);
        // Exit on the first write. If redirection fails, report failure.
        format!("{CMD_LOOP} @if exist {release} ({append} & exit 1) & exit 1")
    } else {
        format!(
            "while ! test -f {}; do sleep 0.01; done; {}",
            sh_path(release),
            append(marker, text),
        )
    }
}

/// Write `W` before the wait and `R` after the release file exists.
pub fn append_wake_then_release(marker: &Path, release: &Path) -> String {
    let after_release = append_after_release(marker, "R", release);
    if cfg!(windows) {
        format!("<nul set /p =W>>{}& {after_release}", cmd_path(marker))
    } else {
        format!("{}; {after_release}", append(marker, "W"))
    }
}
