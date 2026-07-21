//! Cross-platform state shared by a tray frontend and the IPC loop.

use std::path::PathBuf;

use dormant_core::rules::StateSnapshot;

use crate::state::IconState;

/// Latest daemon state consumed by a tray frontend.
#[derive(Debug, Clone)]
pub struct TrayState {
    /// Path to the daemon's Unix socket.
    pub socket_path: PathBuf,
    /// Latest snapshot from the daemon (or `None` until the first Status).
    pub snapshot: Option<StateSnapshot>,
    /// Whether the IPC loop currently reports the daemon as unreachable.
    pub unreachable: bool,
    /// The current icon state derived from `snapshot` / `unreachable`.
    pub icon_state: IconState,
}

impl TrayState {
    /// Create a fresh state with the given socket path; everything else
    /// starts as "starting up" / unreachable until the IPC loop lands.
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            snapshot: None,
            unreachable: true,
            icon_state: IconState::Unreachable,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::TrayState;
    use crate::state::IconState;

    #[test]
    fn new_state_is_unreachable_until_first_snapshot() {
        let socket = PathBuf::from("/tmp/dormant-test.sock");
        let state = TrayState::new(socket.clone());
        assert_eq!(state.socket_path, socket);
        assert!(state.snapshot.is_none());
        assert!(state.unreachable);
        assert_eq!(state.icon_state, IconState::Unreachable);
    }
}
