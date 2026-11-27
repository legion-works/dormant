//! Tests for the resize rebuild order without a Wayland connection or an mpv player.

use super::{RebuildBuffers, ScreensaverRebuild};

const CONFIGURED_SIZE: (u32, u32) = (80, 60);
const RENDER_SIZE: (u32, u32) = (96, 76);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    CreatePool,
    ResizePlayer,
    RenderFrame,
}

#[derive(Debug, PartialEq, Eq)]
enum Effect {
    ResetShift,
    EnsureViewport((u32, u32)),
    CreateBuffers,
    CancelTransitionTimer,
    CancelItemLoadTimer,
    ClearTransition,
    ResizePlayer,
    Render(usize),
    Attach,
    Damage,
    Commit,
    Swap,
    FailToBlack(String),
}

#[derive(Clone)]
struct Buffer;

#[derive(Default)]
struct RebuildRecorder {
    has_first_frame: bool,
    fault: Option<Fault>,
    effects: Vec<Effect>,
}

impl ScreensaverRebuild for RebuildRecorder {
    type Surface = ();
    type Viewport = ();
    type Pool = ();
    type Buffer = Buffer;

    fn rebuild_surface(&self) -> Option<Self::Surface> {
        Some(())
    }

    fn reset_rebuild_shift(&mut self) {
        self.effects.push(Effect::ResetShift);
    }

    fn ensure_rebuild_viewport(&mut self, (): &Self::Surface, size: (u32, u32)) {
        self.effects.push(Effect::EnsureViewport(size));
    }

    fn create_buffers(
        &mut self,
        _size: (u32, u32),
        _stride: u32,
    ) -> Result<RebuildBuffers<Self::Pool, Self::Buffer>, String> {
        self.effects.push(Effect::CreateBuffers);
        if self.fault == Some(Fault::CreatePool) {
            return Err("pool unavailable".into());
        }
        Ok(((), [Buffer, Buffer]))
    }

    fn has_rebuild_session(&self) -> bool {
        true
    }

    fn cancel_transition_timer(&mut self) {
        self.effects.push(Effect::CancelTransitionTimer);
    }

    fn cancel_item_load_timer(&mut self) {
        self.effects.push(Effect::CancelItemLoadTimer);
    }

    fn clear_transition(&mut self) {
        self.effects.push(Effect::ClearTransition);
    }

    fn set_output_size(&mut self, _size: (u32, u32)) -> Result<(), String> {
        self.effects.push(Effect::ResizePlayer);
        if self.fault == Some(Fault::ResizePlayer) {
            return Err("resize rejected".into());
        }
        Ok(())
    }

    fn has_first_frame(&self) -> bool {
        self.has_first_frame
    }

    fn render_current_frame(&mut self, (): &Self::Pool, len: usize) -> Result<(), String> {
        self.effects.push(Effect::Render(len));
        if self.fault == Some(Fault::RenderFrame) {
            return Err("render rejected".into());
        }
        Ok(())
    }

    fn attach_damage_commit(&mut self, (): &Self::Surface, _: &Self::Buffer, _size: (u32, u32)) {
        self.effects
            .extend([Effect::Attach, Effect::Damage, Effect::Commit]);
    }

    fn swap_session_buffers(
        &mut self,
        _: RebuildBuffers<Self::Pool, Self::Buffer>,
        (): Self::Viewport,
        _size: (u32, u32),
        _stride: u32,
    ) {
        self.effects.push(Effect::Swap);
    }

    fn fail_rebuild_to_black(&mut self, reason: &str) {
        self.effects.push(Effect::FailToBlack(reason.into()));
    }
}

fn rebuild_with_first_frame() -> Vec<Effect> {
    let mut state = RebuildRecorder {
        has_first_frame: true,
        ..RebuildRecorder::default()
    };
    state.rebuild_screensaver_buffers(CONFIGURED_SIZE, RENDER_SIZE);
    state.effects
}

#[test]
fn rebuild_clears_transition_and_timers_before_render_and_commit() {
    use Effect::{
        Attach, CancelItemLoadTimer, CancelTransitionTimer, ClearTransition, Commit, Render,
    };

    let effects: Vec<_> = rebuild_with_first_frame()
        .into_iter()
        .filter(|effect| {
            matches!(
                effect,
                CancelTransitionTimer
                    | CancelItemLoadTimer
                    | ClearTransition
                    | Render(_)
                    | Attach
                    | Commit
            )
        })
        .collect();

    assert!(
        effects.starts_with(&[CancelTransitionTimer, CancelItemLoadTimer, ClearTransition]),
        "rebuild must cancel both timers and clear the transition before rendering or committing: {effects:?}"
    );
}

#[test]
fn rebuild_renders_first_frame_before_attach_and_commit() {
    let effects: Vec<_> = rebuild_with_first_frame()
        .into_iter()
        .filter(|effect| matches!(effect, Effect::Render(_) | Effect::Attach | Effect::Commit))
        .collect();

    assert_eq!(
        effects,
        [Effect::Render(29_184), Effect::Attach, Effect::Commit],
        "rebuild must render the current picture before attaching or committing"
    );
}

#[test]
fn rebuild_does_not_render_without_first_frame() {
    let mut state = RebuildRecorder::default();
    state.rebuild_screensaver_buffers(CONFIGURED_SIZE, RENDER_SIZE);
    let effects: Vec<_> = state
        .effects
        .into_iter()
        .filter(|effect| matches!(effect, Effect::Render(_) | Effect::Attach | Effect::Commit))
        .collect();

    assert_eq!(
        effects,
        [Effect::Attach, Effect::Commit],
        "rebuild must not render before the first frame"
    );
}

fn assert_rebuild_failure(fault: Option<Fault>, render_size: (u32, u32), reason: &str) {
    let mut state = RebuildRecorder {
        has_first_frame: true,
        fault,
        ..RebuildRecorder::default()
    };
    state.rebuild_screensaver_buffers(CONFIGURED_SIZE, render_size);

    let effects: Vec<_> = state
        .effects
        .into_iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::FailToBlack(_)
                    | Effect::Attach
                    | Effect::Damage
                    | Effect::Commit
                    | Effect::Swap
            )
        })
        .collect();

    assert!(
        effects.contains(&Effect::FailToBlack(reason.into())),
        "rebuild must report the failure and fall back to black: {effects:?}"
    );
    assert_eq!(
        effects,
        [Effect::FailToBlack(reason.into())],
        "failed rebuild must not attach, damage, commit, or swap the replacement buffers"
    );
}

#[test]
fn rebuild_fails_to_black_on_stride_overflow() {
    assert_rebuild_failure(
        None,
        (u32::MAX / 4 + 1, 76),
        "resize rebuild: stride overflow",
    );
}

#[test]
fn rebuild_fails_to_black_on_pool_creation_failure() {
    assert_rebuild_failure(
        Some(Fault::CreatePool),
        RENDER_SIZE,
        "resize rebuild: RawPool::new: pool unavailable",
    );
}

#[test]
fn rebuild_fails_to_black_on_renderer_resize_failure() {
    assert_rebuild_failure(
        Some(Fault::ResizePlayer),
        RENDER_SIZE,
        "resize rebuild: renderer resize: resize rejected",
    );
}

#[test]
fn rebuild_fails_to_black_on_render_failure() {
    assert_rebuild_failure(
        Some(Fault::RenderFrame),
        RENDER_SIZE,
        "resize rebuild: render rejected",
    );
}

#[test]
fn rebuild_commits_before_swapping_session_buffers() {
    let effects: Vec<_> = rebuild_with_first_frame()
        .into_iter()
        .filter(|effect| matches!(effect, Effect::Attach | Effect::Commit | Effect::Swap))
        .collect();

    assert_eq!(
        effects,
        [Effect::Attach, Effect::Commit, Effect::Swap],
        "rebuild must commit before replacing the session buffers"
    );
}

#[test]
fn rebuild_resets_shift_before_ensuring_viewport() {
    let effects = rebuild_with_first_frame();

    assert!(
        effects.starts_with(&[Effect::ResetShift, Effect::EnsureViewport(CONFIGURED_SIZE)]),
        "rebuild must reset shift before ensuring the viewport: {effects:?}"
    );
}
