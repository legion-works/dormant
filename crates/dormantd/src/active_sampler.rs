//! Pure lifecycle rules and capture boundary for active wear sampling.

use async_trait::async_trait;
use dormant_core::config::schema::StreamMode;
use time::OffsetDateTime;

/// Stable fallback reason when sampling needs a new portal grant.
pub const WEAR_SAMPLING_NEEDS_CONSENT: &str = "wear_sampling_needs_consent";
/// Stable fallback reason for a timed-out or rejected consent flow.
pub const WEAR_SAMPLING_CONSENT_TIMEOUT: &str = "wear_sampling_consent_timeout";
/// Stable fallback reason when the portal transport cannot be reached.
pub const WEAR_SAMPLING_PORTAL_UNREACHABLE: &str = "wear_sampling_portal_unreachable";
/// Stable fallback reason for an invalid or revoked portal token.
pub const WEAR_SAMPLING_TOKEN_INVALID: &str = "wear_sampling_token_invalid";
/// Stable fallback reason when a consent binding targets a changed display.
pub const WEAR_SAMPLING_DISPLAY_CHANGED: &str = "wear_sampling_display_changed";
/// Stable fallback reason when a granted stream does not match its display.
pub const WEAR_SAMPLING_WRONG_MONITOR: &str = "wear_sampling_wrong_monitor";
/// Stable fallback reason for a capture failure before the breaker opens.
pub const WEAR_SAMPLING_CAPTURE_FAILED: &str = "wear_sampling_capture_failed";
/// Stable fallback reason while the capture circuit breaker is open.
pub const WEAR_SAMPLING_COOLDOWN: &str = "wear_sampling_cooldown";
/// Stable fallback reason while administrative settings suspend sampling.
pub const WEAR_SAMPLING_SUSPENDED: &str = "wear_sampling_suspended";

/// Lifecycle state of the active sampling service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingState {
    /// Sampling is disabled by configuration.
    Disabled,
    /// Sampling needs an explicit portal consent grant.
    NeedsConsent,
    /// An explicit portal consent request is in progress.
    ConsentPending,
    /// A saved consent token is being reattached.
    Connecting,
    /// The capture session is available for periodic samples.
    Streaming,
    /// Repeated capture failures have opened the circuit breaker.
    Cooldown,
    /// Sampling is administratively paused while retaining consent.
    Suspended,
}

/// Relevant active-sampling configuration change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDelta {
    /// Active sampling was enabled or disabled.
    Enabled(bool),
    /// The configured sampled display changed.
    SampledDisplayChanged,
    /// The capture stream strategy changed.
    StreamModeChanged,
    /// Capture timeout, failure threshold, or cooldown duration changed.
    LimitsChanged,
    /// The global wear tracker was enabled or disabled.
    WearEnabled(bool),
    /// The configured display appeared or disappeared in a new generation.
    DisplayPresent(bool),
}

/// Event consumed by the sampler lifecycle table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A relevant configuration value changed.
    ConfigChanged(ConfigDelta),
    /// An explicit consent flow was requested.
    GrantStarted,
    /// The portal completed an explicit consent flow.
    Granted,
    /// The operator declined a consent request.
    ConsentDenied,
    /// The consent request exceeded its deadline.
    ConsentTimedOut,
    /// A saved consent token successfully connected.
    Connected,
    /// One capture completed successfully.
    CaptureOk,
    /// The failure threshold was reached, or a cooldown retry failed again.
    CaptureFailed,
    /// The portal reported an authentication or permission failure.
    AuthFailed,
    /// The portal transport failed without invalidating consent.
    TransportFailed,
    /// The portal closed the active capture session.
    SessionClosed,
    /// The circuit-breaker timer elapsed.
    CooldownElapsed,
    /// The operator requested removal of the consent record.
    Forget,
}

/// Work the asynchronous sampler shell performs after a state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Start the explicit portal consent flow.
    OpenConsent,
    /// Reattach to a saved portal consent record.
    Connect,
    /// Request one frame from the current capture session.
    Capture,
    /// Close the active portal session.
    CloseSession,
    /// Cancel an in-flight explicit consent flow.
    CancelConsent,
    /// Attribute uniformly with the supplied stable fallback reason.
    EnterUniform(&'static str),
    /// Persist the token returned by a successful reconnect.
    SaveRotatedToken,
}

/// Result of a pure lifecycle decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// State after the triggering event.
    pub next: SamplingState,
    /// Shell actions needed to realize the transition.
    pub effects: Vec<Effect>,
}

/// Borrowed consent record data needed to open a saved portal stream.
#[derive(Debug, Clone, Copy)]
pub struct ConsentBinding<'a> {
    /// Opaque portal restore token.
    pub token: &'a str,
    /// Display selected when the grant was recorded.
    pub sampled_display: &'a str,
    /// Persistent portal identities observed for the granted display.
    pub portal_persistent_ids: &'a [String],
    /// Width reported by the portal at grant time.
    pub granted_width: u32,
    /// Height reported by the portal at grant time.
    pub granted_height: u32,
}

/// Display identity used to request a fresh portal grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayExpectation {
    /// Configured display identity.
    pub display: String,
}

/// Metadata from a connected portal stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedStream {
    /// Portal-private `PipeWire` node identifier.
    pub node_id: u32,
    /// Token to persist for a subsequent reconnect.
    pub restore_token: String,
    /// Portal persistent identity when the compositor provides one.
    pub persistent_id: Option<String>,
    /// Stream width reported by the portal.
    pub width: u32,
    /// Stream height reported by the portal.
    pub height: u32,
}

/// Successful explicit portal grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Stream metadata returned by the portal.
    pub stream: ConnectedStream,
    /// Wall-clock timestamp used only for consent-record status reporting.
    pub granted_at: OffsetDateTime,
}

/// Raw portal frame before privacy-preserving block reduction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Packed RGBA bytes, including per-row padding when `stride` exceeds width.
    pub rgba: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Number of bytes between adjacent rows.
    pub stride: usize,
}

/// Error returned by a portal capture operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// The saved grant is invalid or lacks permission.
    Auth,
    /// A recoverable portal or `PipeWire` transport failure.
    Transport(String),
    /// The compositor closed the portal capture session.
    SessionClosed,
    /// An unexpected protocol-level error.
    Protocol(String),
    /// The operation exceeded the configured capture deadline.
    Timeout,
}

/// Asynchronous boundary between the portable sampler and platform capture.
#[async_trait]
pub trait CaptureSource: Send + Sync + 'static {
    /// Reattach to a previously granted portal session.
    async fn connect(
        &mut self,
        binding: &ConsentBinding<'_>,
    ) -> Result<ConnectedStream, CaptureError>;

    /// Request a fresh portal consent grant for one display.
    async fn request_consent(
        &mut self,
        display: &DisplayExpectation,
    ) -> Result<Grant, CaptureError>;

    /// Capture one frame using the configured stream strategy.
    async fn capture_one(&mut self, mode: StreamMode) -> Result<RawFrame, CaptureError>;

    /// Release any live portal session.
    async fn close(&mut self);
}

/// Decide the next lifecycle state without performing portal I/O.
///
/// `has_consent_record` is ambient persistent state: it distinguishes a safe
/// reconnect from a fresh-consent requirement after enabling or resuming.
#[must_use]
pub fn decide(state: SamplingState, trigger: Trigger, has_consent_record: bool) -> Transition {
    match trigger {
        Trigger::ConfigChanged(ConfigDelta::Enabled(false)) => disabled(state),
        Trigger::ConfigChanged(ConfigDelta::SampledDisplayChanged) => invalidate_binding(state),
        Trigger::ConfigChanged(
            ConfigDelta::WearEnabled(false) | ConfigDelta::DisplayPresent(false),
        ) => suspended(state),
        Trigger::ConfigChanged(ConfigDelta::Enabled(true)) if state == SamplingState::Disabled => {
            resume(has_consent_record)
        }
        Trigger::ConfigChanged(
            ConfigDelta::WearEnabled(true) | ConfigDelta::DisplayPresent(true),
        ) if state == SamplingState::Suspended => resume(has_consent_record),
        Trigger::GrantStarted if state == SamplingState::NeedsConsent => {
            transition(SamplingState::ConsentPending, vec![Effect::OpenConsent])
        }
        Trigger::Granted if state == SamplingState::ConsentPending => {
            transition(SamplingState::Connecting, vec![Effect::Connect])
        }
        Trigger::ConsentDenied | Trigger::ConsentTimedOut
            if state == SamplingState::ConsentPending =>
        {
            transition(
                SamplingState::NeedsConsent,
                vec![Effect::EnterUniform(WEAR_SAMPLING_CONSENT_TIMEOUT)],
            )
        }
        Trigger::Forget if state == SamplingState::ConsentPending => transition(
            SamplingState::NeedsConsent,
            vec![
                Effect::CancelConsent,
                Effect::EnterUniform(WEAR_SAMPLING_NEEDS_CONSENT),
            ],
        ),
        Trigger::Connected if state == SamplingState::Connecting => transition(
            SamplingState::Streaming,
            vec![Effect::SaveRotatedToken, Effect::Capture],
        ),
        Trigger::TransportFailed if state == SamplingState::Connecting => transition(
            SamplingState::Connecting,
            vec![Effect::EnterUniform(WEAR_SAMPLING_PORTAL_UNREACHABLE)],
        ),
        Trigger::AuthFailed | Trigger::SessionClosed if state == SamplingState::Connecting => {
            needs_consent(state, WEAR_SAMPLING_TOKEN_INVALID)
        }
        Trigger::CaptureOk if state == SamplingState::Streaming => transition(state, vec![]),
        Trigger::CaptureFailed if state == SamplingState::Streaming => transition(
            SamplingState::Cooldown,
            vec![Effect::EnterUniform(WEAR_SAMPLING_CAPTURE_FAILED)],
        ),
        Trigger::AuthFailed | Trigger::SessionClosed if state == SamplingState::Streaming => {
            needs_consent(state, WEAR_SAMPLING_TOKEN_INVALID)
        }
        Trigger::TransportFailed if state == SamplingState::Streaming => transition(
            SamplingState::Connecting,
            vec![Effect::EnterUniform(WEAR_SAMPLING_PORTAL_UNREACHABLE)],
        ),
        Trigger::CooldownElapsed if state == SamplingState::Cooldown => {
            transition(SamplingState::Cooldown, vec![Effect::Capture])
        }
        Trigger::CaptureOk if state == SamplingState::Cooldown => {
            transition(SamplingState::Streaming, vec![])
        }
        Trigger::TransportFailed if state == SamplingState::Cooldown => transition(
            SamplingState::Connecting,
            vec![Effect::EnterUniform(WEAR_SAMPLING_PORTAL_UNREACHABLE)],
        ),
        Trigger::AuthFailed | Trigger::SessionClosed if state == SamplingState::Cooldown => {
            needs_consent(state, WEAR_SAMPLING_TOKEN_INVALID)
        }
        Trigger::CaptureFailed if state == SamplingState::Cooldown => transition(
            SamplingState::Cooldown,
            vec![Effect::EnterUniform(WEAR_SAMPLING_COOLDOWN)],
        ),
        Trigger::ConfigChanged(ConfigDelta::LimitsChanged) if state == SamplingState::Cooldown => {
            transition(SamplingState::Streaming, vec![Effect::Capture])
        }
        _ => transition(state, vec![]),
    }
}

fn disabled(state: SamplingState) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    if state != SamplingState::Disabled {
        effects.push(Effect::CloseSession);
    }
    transition(SamplingState::Disabled, effects)
}

fn needs_consent(state: SamplingState, reason: &'static str) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    effects.push(Effect::EnterUniform(reason));
    transition(SamplingState::NeedsConsent, effects)
}

fn invalidate_binding(state: SamplingState) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    if matches!(
        state,
        SamplingState::Connecting | SamplingState::Streaming | SamplingState::Cooldown
    ) {
        effects.push(Effect::CloseSession);
    }
    effects.push(Effect::EnterUniform(WEAR_SAMPLING_DISPLAY_CHANGED));
    transition(SamplingState::NeedsConsent, effects)
}

fn suspended(state: SamplingState) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    if matches!(
        state,
        SamplingState::Connecting | SamplingState::Streaming | SamplingState::Cooldown
    ) {
        effects.push(Effect::CloseSession);
    }
    effects.push(Effect::EnterUniform(WEAR_SAMPLING_SUSPENDED));
    transition(SamplingState::Suspended, effects)
}

fn resume(has_consent_record: bool) -> Transition {
    if has_consent_record {
        transition(SamplingState::Connecting, vec![Effect::Connect])
    } else {
        transition(
            SamplingState::NeedsConsent,
            vec![Effect::EnterUniform(WEAR_SAMPLING_NEEDS_CONSENT)],
        )
    }
}

fn transition(next: SamplingState, effects: Vec<Effect>) -> Transition {
    Transition { next, effects }
}

#[cfg(test)]
use std::collections::VecDeque;

#[cfg(test)]
#[derive(Debug)]
enum ScriptedOutcome<T> {
    Ready(Result<T, CaptureError>),
    Pending,
}

/// Capture boundary fake with deterministic queued outcomes for lifecycle tests.
#[cfg(test)]
#[derive(Debug, Default)]
struct ScriptedCaptureSource {
    connections: VecDeque<ScriptedOutcome<ConnectedStream>>,
    grants: VecDeque<ScriptedOutcome<Grant>>,
    frames: VecDeque<ScriptedOutcome<RawFrame>>,
    close_calls: usize,
}

#[cfg(test)]
impl ScriptedCaptureSource {
    fn with_frames(outcomes: impl IntoIterator<Item = Result<RawFrame, CaptureError>>) -> Self {
        Self {
            frames: outcomes.into_iter().map(ScriptedOutcome::Ready).collect(),
            ..Self::default()
        }
    }

    fn with_pending_capture() -> Self {
        Self {
            frames: VecDeque::from([ScriptedOutcome::Pending]),
            ..Self::default()
        }
    }

    fn close_calls(&self) -> usize {
        self.close_calls
    }
}

#[cfg(test)]
async fn next_scripted<T>(queue: &mut VecDeque<ScriptedOutcome<T>>) -> Result<T, CaptureError> {
    match queue.front() {
        Some(ScriptedOutcome::Pending) => std::future::pending().await,
        Some(ScriptedOutcome::Ready(_)) => match queue.pop_front() {
            Some(ScriptedOutcome::Ready(outcome)) => outcome,
            Some(ScriptedOutcome::Pending) | Option::None => unreachable!("checked queued outcome"),
        },
        Option::None => Err(CaptureError::Protocol("script exhausted".to_owned())),
    }
}

#[cfg(test)]
#[async_trait]
impl CaptureSource for ScriptedCaptureSource {
    async fn connect(
        &mut self,
        _binding: &ConsentBinding<'_>,
    ) -> Result<ConnectedStream, CaptureError> {
        next_scripted(&mut self.connections).await
    }

    async fn request_consent(
        &mut self,
        _display: &DisplayExpectation,
    ) -> Result<Grant, CaptureError> {
        next_scripted(&mut self.grants).await
    }

    async fn capture_one(&mut self, _mode: StreamMode) -> Result<RawFrame, CaptureError> {
        next_scripted(&mut self.frames).await
    }

    async fn close(&mut self) {
        self.close_calls += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TransitionCase {
        name: &'static str,
        state: SamplingState,
        trigger: Trigger,
        has_consent_record: bool,
        next: SamplingState,
        effects: Vec<Effect>,
    }

    #[test]
    // The full table stays together so state×trigger coverage remains auditable.
    #[allow(clippy::too_many_lines)]
    fn decide_covers_every_specified_lifecycle_exit() {
        let cases = [
            TransitionCase {
                name: "disabled enabling without a record requires consent",
                state: SamplingState::Disabled,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(true)),
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_needs_consent")],
            },
            TransitionCase {
                name: "disabled enabling with a record reconnects",
                state: SamplingState::Disabled,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(true)),
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "needs consent starts only an explicit grant flow",
                state: SamplingState::NeedsConsent,
                trigger: Trigger::GrantStarted,
                has_consent_record: false,
                next: SamplingState::ConsentPending,
                effects: vec![Effect::OpenConsent],
            },
            TransitionCase {
                name: "pending grant connects after a grant",
                state: SamplingState::ConsentPending,
                trigger: Trigger::Granted,
                has_consent_record: false,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "pending consent denial returns to uniform consent fallback",
                state: SamplingState::ConsentPending,
                trigger: Trigger::ConsentDenied,
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_consent_timeout")],
            },
            TransitionCase {
                name: "pending consent timeout returns to uniform consent fallback",
                state: SamplingState::ConsentPending,
                trigger: Trigger::ConsentTimedOut,
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_consent_timeout")],
            },
            TransitionCase {
                name: "forget cancels a pending consent request",
                state: SamplingState::ConsentPending,
                trigger: Trigger::Forget,
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CancelConsent,
                    Effect::EnterUniform("wear_sampling_needs_consent"),
                ],
            },
            TransitionCase {
                name: "connection success starts capture and persists a rotated token",
                state: SamplingState::Connecting,
                trigger: Trigger::Connected,
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![Effect::SaveRotatedToken, Effect::Capture],
            },
            TransitionCase {
                name: "portal unreachable keeps reconnecting with a tagged uniform fallback",
                state: SamplingState::Connecting,
                trigger: Trigger::TransportFailed,
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::EnterUniform("wear_sampling_portal_unreachable")],
            },
            TransitionCase {
                name: "token rejection requires fresh consent",
                state: SamplingState::Connecting,
                trigger: Trigger::AuthFailed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "stream capture failure opens the breaker",
                state: SamplingState::Streaming,
                trigger: Trigger::CaptureFailed,
                has_consent_record: true,
                next: SamplingState::Cooldown,
                effects: vec![Effect::EnterUniform("wear_sampling_capture_failed")],
            },
            TransitionCase {
                name: "stream session closure invalidates consent",
                state: SamplingState::Streaming,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "stream transport failure reconnects without discarding consent",
                state: SamplingState::Streaming,
                trigger: Trigger::TransportFailed,
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::EnterUniform("wear_sampling_portal_unreachable")],
            },
            TransitionCase {
                name: "cooldown expiry retries capture on the existing session",
                state: SamplingState::Cooldown,
                trigger: Trigger::CooldownElapsed,
                has_consent_record: true,
                next: SamplingState::Cooldown,
                effects: vec![Effect::Capture],
            },
            TransitionCase {
                name: "cooldown capture success resumes streaming",
                state: SamplingState::Cooldown,
                trigger: Trigger::CaptureOk,
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![],
            },
            TransitionCase {
                name: "cooldown transport failure reconnects",
                state: SamplingState::Cooldown,
                trigger: Trigger::TransportFailed,
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::EnterUniform("wear_sampling_portal_unreachable")],
            },
            TransitionCase {
                name: "cooldown auth failure requires fresh consent",
                state: SamplingState::Cooldown,
                trigger: Trigger::AuthFailed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "renewed cooldown failure restarts the tagged cooldown",
                state: SamplingState::Cooldown,
                trigger: Trigger::CaptureFailed,
                has_consent_record: true,
                next: SamplingState::Cooldown,
                effects: vec![Effect::EnterUniform("wear_sampling_cooldown")],
            },
            TransitionCase {
                name: "reload resets cooldown and resumes capture",
                state: SamplingState::Cooldown,
                trigger: Trigger::ConfigChanged(ConfigDelta::LimitsChanged),
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![Effect::Capture],
            },
            TransitionCase {
                name: "wear re-enable resumes a retained consent record",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::WearEnabled(true)),
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "wear re-enable without a record stays outside the consent flow",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::WearEnabled(true)),
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_needs_consent")],
            },
            TransitionCase {
                name: "restored display resumes a retained consent record",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::DisplayPresent(true)),
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "restored display without a record remains uniform",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::DisplayPresent(true)),
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_needs_consent")],
            },
            TransitionCase {
                name: "enabled config turns every active state off and closes the session",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "display changes invalidate the consent binding",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::SampledDisplayChanged),
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_display_changed"),
                ],
            },
            TransitionCase {
                name: "wear disable suspends sampling and retains the record",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::WearEnabled(false)),
                has_consent_record: true,
                next: SamplingState::Suspended,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_suspended"),
                ],
            },
            TransitionCase {
                name: "missing display suspends sampling and retains the record",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::DisplayPresent(false)),
                has_consent_record: true,
                next: SamplingState::Suspended,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_suspended"),
                ],
            },
            TransitionCase {
                name: "stream capture success preserves streaming without a fallback effect",
                state: SamplingState::Streaming,
                trigger: Trigger::CaptureOk,
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![],
            },
            TransitionCase {
                name: "connecting session closure invalidates consent",
                state: SamplingState::Connecting,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "cooldown session closure invalidates consent",
                state: SamplingState::Cooldown,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "disabling a pending flow cancels consent before closing the session",
                state: SamplingState::ConsentPending,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: false,
                next: SamplingState::Disabled,
                effects: vec![Effect::CancelConsent, Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling while waiting for consent closes the sampler boundary",
                state: SamplingState::NeedsConsent,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: false,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling while reconnecting closes the sampler boundary",
                state: SamplingState::Connecting,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling during cooldown closes the sampler boundary",
                state: SamplingState::Cooldown,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling a suspended sampler stays disabled",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
        ];

        for case in cases {
            let transition = decide(case.state, case.trigger, case.has_consent_record);
            assert_eq!(transition.next, case.next, "{} next state", case.name);
            assert_eq!(transition.effects, case.effects, "{} effects", case.name);
        }
    }

    #[test]
    fn decide_pins_every_uniform_reason_literal() {
        assert_eq!(WEAR_SAMPLING_NEEDS_CONSENT, "wear_sampling_needs_consent");
        assert_eq!(
            WEAR_SAMPLING_CONSENT_TIMEOUT,
            "wear_sampling_consent_timeout"
        );
        assert_eq!(
            WEAR_SAMPLING_PORTAL_UNREACHABLE,
            "wear_sampling_portal_unreachable"
        );
        assert_eq!(WEAR_SAMPLING_TOKEN_INVALID, "wear_sampling_token_invalid");
        assert_eq!(
            WEAR_SAMPLING_DISPLAY_CHANGED,
            "wear_sampling_display_changed"
        );
        assert_eq!(WEAR_SAMPLING_WRONG_MONITOR, "wear_sampling_wrong_monitor");
        assert_eq!(WEAR_SAMPLING_CAPTURE_FAILED, "wear_sampling_capture_failed");
        assert_eq!(WEAR_SAMPLING_COOLDOWN, "wear_sampling_cooldown");
        assert_eq!(WEAR_SAMPLING_SUSPENDED, "wear_sampling_suspended");
    }

    #[tokio::test]
    async fn scripted_capture_outcomes_are_consumed_in_order() {
        let mut source = ScriptedCaptureSource::with_frames([
            Ok(RawFrame {
                rgba: vec![0, 0, 0, 255],
                width: 1,
                height: 1,
                stride: 4,
            }),
            Err(CaptureError::Auth),
            Err(CaptureError::Transport("portal reset".to_owned())),
            Err(CaptureError::SessionClosed),
        ]);

        assert!(source.capture_one(StreamMode::Warm).await.is_ok());
        assert_eq!(
            source.capture_one(StreamMode::Warm).await,
            Err(CaptureError::Auth)
        );
        assert_eq!(
            source.capture_one(StreamMode::Warm).await,
            Err(CaptureError::Transport("portal reset".to_owned()))
        );
        assert_eq!(
            source.capture_one(StreamMode::Warm).await,
            Err(CaptureError::SessionClosed)
        );
    }

    #[tokio::test]
    async fn scripted_capture_grants_and_connections_are_consumed_in_order() {
        let stream = ConnectedStream {
            node_id: 7,
            restore_token: "rotated".to_owned(),
            persistent_id: Some("panel-7".to_owned()),
            width: 1920,
            height: 1080,
        };
        let grant = Grant {
            stream: stream.clone(),
            granted_at: OffsetDateTime::UNIX_EPOCH,
        };
        let mut source = ScriptedCaptureSource {
            connections: VecDeque::from([ScriptedOutcome::Ready(Ok(stream.clone()))]),
            grants: VecDeque::from([ScriptedOutcome::Ready(Ok(grant.clone()))]),
            ..ScriptedCaptureSource::default()
        };
        let binding = ConsentBinding {
            token: "saved",
            sampled_display: "oled",
            portal_persistent_ids: &[],
            granted_width: 1920,
            granted_height: 1080,
        };
        let display = DisplayExpectation {
            display: "oled".to_owned(),
        };

        assert_eq!(source.connect(&binding).await, Ok(stream));
        assert_eq!(source.request_consent(&display).await, Ok(grant));
    }

    #[tokio::test]
    async fn scripted_capture_pending_future_can_be_cancelled_before_close() {
        let mut source = ScriptedCaptureSource::with_pending_capture();
        {
            let capture = source.capture_one(StreamMode::PerTick);
            tokio::pin!(capture);

            let waker = std::task::Waker::noop();
            let mut context = std::task::Context::from_waker(waker);
            assert!(matches!(
                std::future::Future::poll(capture.as_mut(), &mut context),
                std::task::Poll::Pending
            ));
        }

        source.close().await;
        assert_eq!(source.close_calls(), 1);
    }
}
