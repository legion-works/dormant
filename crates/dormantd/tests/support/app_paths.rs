//! Per-app filesystem ownership for daemon integration tests.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Paths owned by one test app for its entire lifetime.
#[allow(
    dead_code,
    reason = "each integration binary uses a subset of the shared literal paths"
)]
pub struct TestAppPaths {
    root: TempDir,
    /// Configuration file consumed by the app.
    pub config: PathBuf,
    /// Credentials file consumed by the app.
    pub credentials: PathBuf,
    /// Daemon state root; wear ledgers live beneath this directory.
    pub state: PathBuf,
    /// Unix IPC listener path.
    pub ipc_socket: PathBuf,
    /// systemd notify listener path.
    pub notify_socket: PathBuf,
    /// Command-controller marker path.
    pub marker: PathBuf,
    /// Test-local diagnostic capture path.
    pub captures: PathBuf,
}

impl TestAppPaths {
    /// Create a non-overlapping set of literal child paths.
    pub fn new() -> Self {
        let root = TempDir::new().expect("create test app root");
        let path = root.path();
        Self {
            config: path.join("config.toml"),
            credentials: path.join("credentials.toml"),
            state: path.join("state"),
            ipc_socket: path.join("d.sock"),
            notify_socket: path.join("n.sock"),
            marker: path.join("marker"),
            captures: path.join("captures"),
            root,
        }
    }

    /// Root for fixtures that need additional app-owned files.
    #[allow(
        dead_code,
        reason = "only boot integration fixtures need files beyond the literal paths"
    )]
    pub fn root(&self) -> &Path {
        self.root.path()
    }
}
