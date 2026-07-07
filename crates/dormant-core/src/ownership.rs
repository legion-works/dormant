//! Ownership gate — may THIS instance drive a display right now?
//!
//! Feature A ships [`AlwaysOwned`]; feature B (multi-instance coordination) will
//! inject an input-source/MQTT-backed impl without changing the state machine —
//! the machine already consumes `Input::OwnershipChanged`; the engine feeds it
//! from this gate.  This is the seam that makes B a drop-in.

use crate::types::DisplayId;

/// May this instance act on `display`'s power/surface right now?
///
/// The engine consults this gate before feeding
/// [`Input::OwnershipChanged`](crate::state_machine::Input::OwnershipChanged)
/// to the state machine so the machine's `owned` flag reflects the gate.
pub trait OwnershipGate: Send + Sync {
    /// Returns `true` when this instance is allowed to control `display`.
    fn owns(&self, display: &DisplayId) -> bool;
}

/// Single-instance default: owns every display.
///
/// When only one daemon instance runs, it always owns every display it is
/// configured for.  Feature B will replace this with a gate that coordinates
/// across instances (e.g. via MQTT leader election).
pub struct AlwaysOwned;

impl OwnershipGate for AlwaysOwned {
    fn owns(&self, _display: &DisplayId) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_owned_owns_any_display() {
        let gate = AlwaysOwned;
        assert!(gate.owns(&DisplayId("anything".into())));
    }
}
