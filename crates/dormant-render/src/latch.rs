//! The first-input-event latch.
//!
//! A render surface needs to wake the engine on the FIRST pointer/key
//! input event after the surface is shown.  Subsequent events are ignored
//! until the surface is torn down (which `reset`s the latch), so a user
//! typing or wiggling the mouse does not generate a flood of wake events
//! the engine has to dedupe.
//!
//! Kept as a tiny `&mut self` struct so it can be unit-tested without any
//! Wayland state.

use dormant_core::types::DisplayId;

/// A one-shot latch that arms when [`Self::on_input`] is first called and
/// stays consumed until [`Self::reset`] runs.
#[derive(Debug)]
pub(crate) struct FirstInputLatch {
    consumed: bool,
    display_id: DisplayId,
}

impl FirstInputLatch {
    /// Build a fresh latch bound to `display_id`.
    #[must_use]
    pub(crate) fn new(display_id: DisplayId) -> Self {
        Self {
            consumed: false,
            display_id,
        }
    }

    /// Register an input event.  Returns `Some(display_id)` on the FIRST
    /// call after a fresh latch (the caller should emit `InputWake`) and
    /// `None` thereafter.
    #[must_use]
    pub(crate) fn on_input(&mut self) -> Option<DisplayId> {
        if self.consumed {
            None
        } else {
            self.consumed = true;
            Some(self.display_id.clone())
        }
    }

    /// Re-arm the latch so the next input event fires again.
    pub(crate) fn reset(&mut self) {
        self.consumed = false;
    }

    /// True if the latch has already fired since the last reset.  Used
    /// by unit tests to assert the state without going through
    /// [`Self::on_input`] (which consumes the event).
    #[must_use]
    #[allow(dead_code)] // only used by unit tests today
    pub(crate) fn is_consumed(&self) -> bool {
        self.consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn did(s: &str) -> DisplayId {
        DisplayId(s.to_string())
    }

    #[test]
    fn first_event_emits_then_quiet() {
        let mut latch = FirstInputLatch::new(did("display-A"));
        assert_eq!(latch.on_input(), Some(did("display-A")));
        assert_eq!(latch.on_input(), None);
        assert_eq!(latch.on_input(), None);
    }

    #[test]
    fn reset_rearms_latch() {
        let mut latch = FirstInputLatch::new(did("display-A"));
        assert_eq!(latch.on_input(), Some(did("display-A")));
        assert_eq!(latch.on_input(), None);
        latch.reset();
        assert_eq!(latch.on_input(), Some(did("display-A")));
        assert_eq!(latch.on_input(), None);
    }

    #[test]
    fn multiple_resets_only_fire_once_each_window() {
        let mut latch = FirstInputLatch::new(did("display-B"));
        latch.reset();
        latch.reset();
        assert_eq!(latch.on_input(), Some(did("display-B")));
    }

    #[test]
    fn is_consumed_tracks_latch() {
        let mut latch = FirstInputLatch::new(did("display-A"));
        assert!(!latch.is_consumed());
        let _ = latch.on_input();
        assert!(latch.is_consumed());
        latch.reset();
        assert!(!latch.is_consumed());
    }
}
