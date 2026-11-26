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

// Keep the hold in cmd so killing the shell also stops the hold.
const CMD_HOLD: &str = "for /l %i in (1,1,2147483647) do @rem";

/// Hold until the controller cancels the command or its timeout expires.
pub fn hold(path: &Path) -> String {
    if cfg!(windows) {
        CMD_HOLD.to_owned()
    } else {
        format!("exec tail -f {}", sh_path(path))
    }
}

/// Write the start marker before holding the command open.
pub fn append_start_then_hold(marker: &Path, hold_path: &Path) -> String {
    if cfg!(windows) {
        format!("<nul set /p =START>>{}& {CMD_HOLD}", cmd_path(marker))
    } else {
        format!("{}; {}", append(marker, "START"), hold(hold_path))
    }
}
