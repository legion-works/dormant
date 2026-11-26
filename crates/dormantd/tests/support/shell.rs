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
    let path = path.to_str().expect("UTF-8 test marker path");
    if cfg!(windows) {
        // Rust escapes embedded double quotes in arguments. cmd does not
        // recognize those escapes, so the path cannot require quotes.
        assert!(
            !path.contains(|ch: char| ch.is_whitespace() || "\"&|<>()^%!".contains(ch)),
            "Windows test marker path must have no spaces or cmd metacharacters: {path}"
        );
        // set /p prints the prompt but gets EOF from NUL. Apply the output
        // redirection to the group so a failed open cannot reach exit 0.
        format!("(<nul set /p ={text}& exit 0)>>{path}")
    } else {
        let path = path.replace('\'', "'\\''");
        format!("printf %s '{text}' >> '{path}'")
    }
}
