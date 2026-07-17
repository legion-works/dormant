//! Process-shared ownership of the controller that most recently blanked a display.

use std::collections::HashMap;
use std::sync::Mutex;

use dormant_core::types::DisplayId;

/// Daemon-lifetime controller ownership entries shared across executor rebuilds.
#[derive(Default)]
pub struct BlankOwnerRegistry {
    owners: Mutex<HashMap<String, OwnerEntry>>,
}

struct OwnerEntry {
    fingerprint: String,
    controller_index: usize,
}

impl BlankOwnerRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the recorded owner when the controller chain still matches.
    ///
    /// # Panics
    ///
    /// Panics when the registry mutex is poisoned.
    pub fn owner(&self, display: &DisplayId, fingerprint: &str) -> Option<usize> {
        let mut owners = self
            .owners
            .lock()
            .expect("blank owner registry lock poisoned");
        match owners.get(&display.0) {
            Some(entry) if entry.fingerprint == fingerprint => Some(entry.controller_index),
            Some(_) => {
                owners.remove(&display.0);
                None
            }
            None => None,
        }
    }

    /// Record the controller that successfully blanked a display.
    ///
    /// # Panics
    ///
    /// Panics when the registry mutex is poisoned.
    pub fn record(&self, display: &DisplayId, fingerprint: &str, controller_index: usize) {
        self.owners
            .lock()
            .expect("blank owner registry lock poisoned")
            .insert(
                display.to_string(),
                OwnerEntry {
                    fingerprint: fingerprint.to_string(),
                    controller_index,
                },
            );
    }

    /// Clear an entry only when the recorded owner still matches the wake attempt.
    ///
    /// # Panics
    ///
    /// Panics when the registry mutex is poisoned.
    pub fn clear_if_owner(&self, display: &DisplayId, fingerprint: &str, controller_index: usize) {
        let mut owners = self
            .owners
            .lock()
            .expect("blank owner registry lock poisoned");
        if owners.get(&display.0).is_some_and(|entry| {
            entry.fingerprint == fingerprint && entry.controller_index == controller_index
        }) {
            owners.remove(&display.0);
        }
    }
}
