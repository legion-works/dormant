//! Intentionally failing nextest fixture used only by CI policy verification.

#[cfg(test)]
mod tests {
    use std::{env, fs::OpenOptions, io::ErrorKind, path::PathBuf};

    #[test]
    fn fail_once() {
        let marker = PathBuf::from(
            env::var("NEXTEST_POLICY_STATE_DIR").expect("state directory is required"),
        )
        .join("fail-once.marker");
        match OpenOptions::new().write(true).create_new(true).open(marker) {
            Ok(_) => panic!("intentional first-attempt failure"),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => panic!("could not create state marker: {error}"),
        }
    }

    #[test]
    fn always_fails() {
        panic!("intentional permanent failure");
    }

    #[test]
    fn always_passes() {}
}
