//! Daemon reload outcome, shared by `dormantd` (emits it) and `dormant-web`
//! (subscribes to it) — must live in core to avoid a dependency cycle.

/// Outcome of a reload attempt, published on the daemon-level reload bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The new config was applied.
    Reloaded,
    /// The reload was rejected; the old config remains active. Carries a
    /// human-readable detail.
    Rejected(String),
}
