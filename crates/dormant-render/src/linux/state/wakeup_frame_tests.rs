//! Screensaver wakeup effects without an mpv player or a Wayland connection.

use tokio::sync::oneshot;

use super::{
    MpvWakeupFrame, StepCmd, TransitionEvent, TransitionPhase, TransitionState, WakeupFrameOutcome,
    transition_step,
};

const CAPTURE: [u8; 4] = [10, 20, 30, 0];
const RENDER_ERROR: &str = "mpv render failed";
const FRAME_RENDERED: TransitionEvent = TransitionEvent::FrameRendered { ok: true };

#[derive(Debug, PartialEq, Eq)]
enum Effect {
    CancelItemLoadDeadline,
    Render,
    FrameEvent(TransitionEvent, TransitionPhase),
    CancelFadeTimer,
    Blend(Vec<u8>, u16),
    Commit,
    MarkFirstFrame,
    Reply,
    Swap,
}

struct WakeupRecorder {
    transition: Option<TransitionState>,
    busy: bool,
    render_error: Option<&'static str>,
    surface_available: bool,
    has_first_frame: bool,
    first_reply: Option<oneshot::Sender<()>>,
    effects: Vec<Effect>,
}

impl WakeupRecorder {
    fn new(transition: Option<(TransitionPhase, u16)>) -> Self {
        Self {
            transition: transition.map(|(phase, t)| TransitionState {
                capture: CAPTURE.to_vec(),
                phase,
                t,
                t_step: 8,
                timer_token: None,
            }),
            busy: false,
            render_error: None,
            surface_available: true,
            has_first_frame: true,
            first_reply: None,
            effects: Vec::new(),
        }
    }
}

impl MpvWakeupFrame for WakeupRecorder {
    fn transition(&self) -> Option<&TransitionState> {
        self.transition.as_ref()
    }

    fn cancel_item_load_deadline(&mut self) {
        self.effects.push(Effect::CancelItemLoadDeadline);
    }

    fn back_buffer_ready(&self) -> bool {
        !self.busy
    }

    fn render_back_buffer(&mut self) -> Result<(), String> {
        self.effects.push(Effect::Render);
        if let Some(error) = self.render_error {
            return Err(error.to_owned());
        }
        Ok(())
    }

    fn apply_frame_event(&mut self, event: TransitionEvent) -> StepCmd {
        let Some(transition) = self.transition.as_mut() else {
            return StepCmd::NoOp;
        };
        let (phase, _, cmd) = transition_step(transition.phase, transition.t, event);
        transition.phase = phase;
        self.effects.push(Effect::FrameEvent(event, phase));
        cmd
    }

    fn cancel_fade_timer(&mut self) {
        self.effects.push(Effect::CancelFadeTimer);
    }

    fn blend_back_buffer(&mut self, capture: &[u8], t: u16) {
        self.effects.push(Effect::Blend(capture.to_vec(), t));
    }

    fn commit_back_buffer(&mut self) -> bool {
        if self.surface_available {
            self.effects.push(Effect::Commit);
        }
        self.surface_available
    }

    fn finish_first_frame(&mut self) {
        if !self.has_first_frame {
            self.has_first_frame = true;
            self.effects.push(Effect::MarkFirstFrame);
            if let Some(reply) = self.first_reply.take() {
                reply.send(()).expect("first-frame receiver is live");
                self.effects.push(Effect::Reply);
            }
        }
    }

    fn swap_back_buffer(&mut self) {
        self.effects.push(Effect::Swap);
    }
}

#[test]
fn captured_requests_load_timer_without_render_or_commit() {
    let mut frame = WakeupRecorder::new(Some((TransitionPhase::Captured, 0)));

    let outcome = frame.handle_mpv_frame(false);

    assert!(
        frame.effects.is_empty(),
        "Captured must not render or commit before item load: {:?}",
        frame.effects
    );
    assert_eq!(outcome, WakeupFrameOutcome::ArmItemLoadTimer);
}

#[test]
fn pending_fade_arm_leaves_first_commit_to_timer() {
    let mut frame = WakeupRecorder::new(Some((TransitionPhase::Fading, 0)));

    let outcome = frame.handle_mpv_frame(true);

    assert_eq!(
        frame.effects,
        [Effect::CancelItemLoadDeadline],
        "pending fade arm must not render or commit on the wakeup"
    );
    assert_eq!(outcome, WakeupFrameOutcome::ArmTransitionTimer);
}

#[test]
fn busy_back_buffer_is_not_rendered_or_committed() {
    let mut frame = WakeupRecorder::new(None);
    frame.busy = true;

    let outcome = frame.handle_mpv_frame(false);

    assert_eq!(
        frame.effects,
        [Effect::CancelItemLoadDeadline],
        "busy back buffer must not render or commit"
    );
    assert_eq!(outcome, WakeupFrameOutcome::Done);
}

#[test]
fn first_loaded_frame_renders_and_hands_off_to_fade_timer() {
    let mut frame = WakeupRecorder::new(Some((TransitionPhase::AwaitingFirstFrame, 0)));

    let outcome = frame.handle_mpv_frame(false);

    assert_eq!(
        frame.effects,
        [
            Effect::CancelItemLoadDeadline,
            Effect::Render,
            Effect::FrameEvent(FRAME_RENDERED, TransitionPhase::Fading),
            Effect::CancelFadeTimer,
        ],
        "first loaded frame must render and reach Fading before timer handoff"
    );
    assert_eq!(outcome, WakeupFrameOutcome::ArmTransitionTimer);
    assert_eq!(frame.transition.unwrap().phase, TransitionPhase::Fading);
}

#[test]
fn fading_wakeup_blends_at_current_progress_before_commit() {
    for t in [0, 128] {
        let mut frame = WakeupRecorder::new(Some((TransitionPhase::Fading, t)));

        let outcome = frame.handle_mpv_frame(false);

        assert_eq!(
            frame.effects,
            [
                Effect::CancelItemLoadDeadline,
                Effect::Render,
                Effect::FrameEvent(FRAME_RENDERED, TransitionPhase::Fading),
                Effect::Blend(CAPTURE.to_vec(), t),
                Effect::Commit,
                Effect::Swap,
            ],
            "live fade must blend at t={t} before commit"
        );
        assert_eq!(outcome, WakeupFrameOutcome::Done);
        assert_eq!(frame.transition.unwrap().t, t);
    }
}

#[test]
fn wakeup_without_transition_renders_and_commits_without_blend() {
    let mut frame = WakeupRecorder::new(None);

    let outcome = frame.handle_mpv_frame(false);

    assert_eq!(
        frame.effects,
        [
            Effect::CancelItemLoadDeadline,
            Effect::Render,
            Effect::Commit,
            Effect::Swap,
        ],
        "wakeup without a transition must commit the render without blending"
    );
    assert_eq!(outcome, WakeupFrameOutcome::Done);
}

#[test]
fn render_failure_requests_black_without_phase_step_or_commit() {
    let mut frame = WakeupRecorder::new(Some((TransitionPhase::AwaitingFirstFrame, 0)));
    frame.render_error = Some(RENDER_ERROR);

    let outcome = frame.handle_mpv_frame(false);

    assert_eq!(
        frame.effects,
        [Effect::CancelItemLoadDeadline, Effect::Render],
        "render failure must not step the phase or commit"
    );
    assert_eq!(
        outcome,
        WakeupFrameOutcome::FailToBlack(RENDER_ERROR.to_owned())
    );
}

#[test]
fn first_frame_sets_flag_and_replies_only_after_commit() {
    let mut frame = WakeupRecorder::new(None);
    let (reply, mut received) = oneshot::channel();
    frame.has_first_frame = false;
    frame.first_reply = Some(reply);
    frame.surface_available = false;

    let outcome = frame.handle_mpv_frame(false);

    assert!(
        !frame.has_first_frame,
        "first frame must not set the flag or reply before a commit"
    );
    assert_eq!(
        received.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    );
    assert_eq!(outcome, WakeupFrameOutcome::Done);

    frame.effects.clear();
    frame.surface_available = true;
    let outcome = frame.handle_mpv_frame(false);

    assert_eq!(
        frame.effects,
        [
            Effect::CancelItemLoadDeadline,
            Effect::Render,
            Effect::Commit,
            Effect::MarkFirstFrame,
            Effect::Reply,
            Effect::Swap,
        ],
        "first-frame flag and reply must follow the commit"
    );
    assert!(frame.has_first_frame);
    assert_eq!(received.try_recv(), Ok(()));
    assert_eq!(outcome, WakeupFrameOutcome::Done);
}
