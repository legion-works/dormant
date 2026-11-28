//! Crossfade tick effects without an mpv player or a Wayland connection.

use super::TransitionTickOutcome::{Complete, Done, FailToBlack};
use super::{T_MAX, TransitionMode, TransitionPhase, TransitionState, TransitionTick};

const CAPTURE: [u8; 4] = [10, 20, 30, 0];
const GENERATION: u64 = 17;
const STEP: u16 = 8;

#[derive(Debug, PartialEq, Eq)]
enum Effect {
    Render,
    Blend(Vec<u8>, u16),
    Commit,
    Swap,
    CancelFadeTimer(TransitionPhase, u16),
}

struct TickRecorder {
    identity: Option<(u64, TransitionMode)>,
    transition: Option<TransitionState>,
    busy: bool,
    render_error: Option<&'static str>,
    commit_succeeds: bool,
    effects: Vec<Effect>,
}

impl TickRecorder {
    fn fading(t: u16) -> Self {
        Self {
            identity: Some((GENERATION, TransitionMode::Crossfade)),
            transition: Some(TransitionState {
                capture: CAPTURE.to_vec(),
                phase: TransitionPhase::Fading,
                t,
                t_step: STEP,
                timer_token: None,
            }),
            busy: false,
            render_error: None,
            commit_succeeds: true,
            effects: Vec::new(),
        }
    }

    fn progress(&self) -> Option<(TransitionPhase, u16)> {
        self.transition.as_ref().map(|tr| (tr.phase, tr.t))
    }
}

impl TransitionTick for TickRecorder {
    fn session_identity(&self) -> Option<(u64, TransitionMode)> {
        self.identity
    }

    fn transition(&self) -> Option<&TransitionState> {
        self.transition.as_ref()
    }

    fn transition_mut(&mut self) -> Option<&mut TransitionState> {
        self.transition.as_mut()
    }

    fn back_buffer_ready(&self) -> bool {
        !self.busy
    }

    fn render_back_buffer(&mut self) -> Result<(), String> {
        self.effects.push(Effect::Render);
        self.render_error
            .map_or(Ok(()), |error| Err(error.to_owned()))
    }

    fn blend_back_buffer(&mut self, capture: &[u8], t: u16) {
        self.effects.push(Effect::Blend(capture.to_vec(), t));
    }

    fn commit_back_buffer(&mut self) -> bool {
        self.effects.push(Effect::Commit);
        self.commit_succeeds
    }

    fn swap_back_buffer(&mut self) {
        self.effects.push(Effect::Swap);
    }

    fn cancel_fade_timer(&mut self) {
        let (phase, t) = self.progress().expect("the fade has transition state");
        self.effects.push(Effect::CancelFadeTimer(phase, t));
    }
}

#[test]
fn tick_without_matching_crossfade_session_does_nothing() {
    for (identity, case) in [
        (
            Some((GENERATION + 1, TransitionMode::Crossfade)),
            "stale generation",
        ),
        (
            Some((GENERATION, TransitionMode::None)),
            "non-crossfade mode",
        ),
        (None, "no session"),
    ] {
        let mut tick = TickRecorder::fading(0);
        tick.identity = identity;
        if identity.is_none() {
            tick.transition = None;
        }
        let before = tick.progress();

        let outcome = tick.handle_transition_tick(GENERATION);

        assert!(
            tick.effects.is_empty(),
            "{case} must have no effects: {:?}",
            tick.effects
        );
        assert_eq!(tick.progress(), before, "{case} must not move progress");
        assert_eq!(outcome, Done);
    }
}

#[test]
fn busy_tick_does_not_render_or_advance() {
    let mut tick = TickRecorder::fading(0);
    tick.busy = true;

    let outcome = tick.handle_transition_tick(GENERATION);

    assert!(
        tick.effects.is_empty(),
        "busy tick must not render or commit: {:?}",
        tick.effects
    );
    assert_eq!(tick.progress(), Some((TransitionPhase::Fading, 0)));
    assert_eq!(outcome, Done);
}

#[test]
fn first_tick_blends_capture_at_zero_before_commit() {
    let mut tick = TickRecorder::fading(0);

    let outcome = tick.handle_transition_tick(GENERATION);

    assert_eq!(
        tick.effects,
        [
            Effect::Render,
            Effect::Blend(CAPTURE.to_vec(), 0),
            Effect::Commit,
            Effect::Swap
        ],
        "first tick must blend at zero before commit"
    );
    assert_eq!(tick.progress(), Some((TransitionPhase::Fading, STEP)));
    assert_eq!(outcome, Done);
}

#[test]
fn mid_fade_tick_commits_pre_step_blend_and_advances_progress() {
    let mut tick = TickRecorder::fading(120);

    let outcome = tick.handle_transition_tick(GENERATION);

    assert_eq!(
        tick.effects,
        [
            Effect::Render,
            Effect::Blend(CAPTURE.to_vec(), 120),
            Effect::Commit,
            Effect::Swap
        ],
        "mid-fade tick must commit at the pre-step progress"
    );
    assert_eq!(tick.progress(), Some((TransitionPhase::Fading, 128)));
    assert_eq!(outcome, Done);
}

#[test]
fn terminal_tick_commits_unblended_frame_then_cancels_once() {
    let mut tick = TickRecorder::fading(T_MAX - 3);

    let outcome = tick.handle_transition_tick(GENERATION);

    assert_eq!(
        tick.effects,
        [
            Effect::Render,
            Effect::Commit,
            Effect::Swap,
            Effect::CancelFadeTimer(TransitionPhase::Idle, T_MAX),
        ],
        "terminal tick must commit the new frame without a blend, then cancel the timer at Idle"
    );
    assert_eq!(outcome, Complete);
    assert_eq!(tick.progress(), Some((TransitionPhase::Idle, T_MAX)));

    tick.effects.clear();
    let outcome = tick.handle_transition_tick(GENERATION);

    assert!(
        !tick
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::CancelFadeTimer(..))),
        "a later tick at Idle must not cancel the timer again"
    );
    assert_eq!(outcome, Done);
    assert_eq!(tick.progress(), Some((TransitionPhase::Idle, T_MAX)));
}

#[test]
fn terminal_tick_without_surface_cancels_timer_once() {
    let mut tick = TickRecorder::fading(T_MAX - 3);
    tick.commit_succeeds = false;

    assert_eq!(tick.handle_transition_tick(GENERATION), Done);
    assert_eq!(tick.progress(), Some((TransitionPhase::Idle, T_MAX)));
    assert_eq!(
        tick.effects
            .iter()
            .filter(|effect| matches!(effect, Effect::CancelFadeTimer(..)))
            .count(),
        1,
        "terminal tick must cancel the fade timer even without a surface"
    );

    assert_eq!(tick.handle_transition_tick(GENERATION), Done);
    assert_eq!(
        tick.effects,
        [
            Effect::Render,
            Effect::Commit,
            Effect::CancelFadeTimer(TransitionPhase::Idle, T_MAX),
            Effect::Render,
            Effect::Commit,
        ],
        "missing surface must prevent a swap and the later tick must not cancel again"
    );
}

#[test]
fn tick_render_error_requests_black_without_step_or_commit() {
    let mut tick = TickRecorder::fading(T_MAX - 3);
    tick.render_error = Some("mpv render failed");

    let outcome = tick.handle_transition_tick(GENERATION);

    assert_eq!(
        tick.effects,
        [Effect::Render],
        "render failure must not blend, commit, or cancel"
    );
    assert_eq!(
        tick.progress(),
        Some((TransitionPhase::Fading, T_MAX - 3)),
        "render failure must not step the phase"
    );
    assert_eq!(outcome, FailToBlack("mpv render failed".to_owned()));
}
