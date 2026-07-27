//! Internal probe result types for the `dormant-doctor` crate.
//!
//! These are the working types used by individual probe functions.  The public
//! API boundary maps them to `dormant_core::doctor::Check` / `DoctorReport`
//! via [`super::to_report`].

/// The outcome of a single probe.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    /// Human-readable probe name (e.g. `"ddcci"`, `"usb /dev/ttyUSB0"`).
    pub name: String,
    /// Probe status.
    pub status: ProbeStatus,
    /// Optional detail message.
    pub detail: String,
    /// Check category — `"config"` | `"sensor"` | `"display"` | `"platform"` | `"network"`.
    pub category: Option<String>,
    /// The entity this check is about — display id, sensor id, or None.
    pub subject: Option<String>,
}

/// Probe status.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeStatus {
    /// Probe succeeded.
    Pass,
    /// Probe failed.
    Fail,
    /// Probe was skipped (no applicable config).
    Skip,
    /// The probe is not supported on this platform or in this release.
    NotSupported,
}

impl ProbeResult {
    /// Create a passing probe result.
    #[must_use]
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProbeStatus::Pass,
            detail: detail.into(),
            category: None,
            subject: None,
        }
    }

    /// Create a failing probe result.
    #[must_use]
    pub fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProbeStatus::Fail,
            detail: detail.into(),
            category: None,
            subject: None,
        }
    }

    /// Create a skipped probe result.
    #[must_use]
    pub fn skip(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProbeStatus::Skip,
            detail: detail.into(),
            category: None,
            subject: None,
        }
    }

    /// Create a not-supported probe result.
    #[must_use]
    pub fn not_supported(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProbeStatus::NotSupported,
            detail: detail.into(),
            category: None,
            subject: None,
        }
    }

    /// Set the category on this result (builder-style).
    #[must_use]
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    /// Set the subject on this result (builder-style).
    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }
}
