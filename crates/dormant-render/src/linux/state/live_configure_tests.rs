//! Effects of live configure without a Wayland connection.

use super::LiveConfigure;

#[derive(Debug, PartialEq, Eq)]
enum Effect {
    ConfiguredSize((u32, u32)),
    ViewportDestination((u32, u32)),
    Rebuild((u32, u32), (u32, u32)),
    Commit,
}

#[derive(Default)]
struct ConfigureRecorder {
    session_dims: Option<(u32, u32)>,
    overscan: u32,
    effects: Vec<Effect>,
}

impl LiveConfigure for ConfigureRecorder {
    fn set_configured_size(&mut self, size: (u32, u32)) {
        self.effects.push(Effect::ConfiguredSize(size));
    }

    fn screensaver_dims(&self) -> Option<(u32, u32)> {
        self.session_dims
    }

    fn render_dims(&self, (width, height): (u32, u32)) -> (u32, u32) {
        (width + self.overscan, height + self.overscan)
    }

    fn request_screensaver_rebuild(&mut self, size: (u32, u32), render_size: (u32, u32)) {
        self.effects.push(Effect::Rebuild(size, render_size));
    }

    fn set_viewport_destination(&mut self, size: (u32, u32)) {
        self.effects.push(Effect::ViewportDestination(size));
    }

    fn commit_live_surface(&mut self) {
        self.effects.push(Effect::Commit);
    }
}

#[test]
fn live_configure_requests_rebuild_after_resize() {
    let mut state = ConfigureRecorder {
        session_dims: Some((1920, 1080)),
        ..ConfigureRecorder::default()
    };

    state.reconfigure_live((2560, 1440));

    assert!(
        state
            .effects
            .contains(&Effect::Rebuild((2560, 1440), (2560, 1440))),
        "live resize must request a rebuild at the new dimensions: {:?}",
        state.effects
    );
}

#[test]
fn live_configure_keeps_same_size_buffers() {
    let mut state = ConfigureRecorder {
        session_dims: Some((1928, 1088)),
        overscan: 8,
        ..ConfigureRecorder::default()
    };

    state.reconfigure_live((1920, 1080));

    assert!(
        !state
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Rebuild(..))),
        "live configure must keep buffers at the requested render size: {:?}",
        state.effects
    );
}

#[test]
fn live_configure_updates_black_overlay_without_rebuild() {
    let mut state = ConfigureRecorder::default();

    state.reconfigure_live((2560, 1440));

    assert_eq!(
        state.effects,
        [
            Effect::ConfiguredSize((2560, 1440)),
            Effect::ViewportDestination((2560, 1440)),
            Effect::Commit,
        ],
        "live configure must update and commit the black overlay without a screensaver rebuild"
    );
}

#[test]
fn live_configure_rebuilds_before_commit() {
    let mut state = ConfigureRecorder {
        session_dims: Some((1920, 1080)),
        ..ConfigureRecorder::default()
    };

    state.reconfigure_live((2560, 1440));

    assert_eq!(
        state.effects,
        [
            Effect::ConfiguredSize((2560, 1440)),
            Effect::Rebuild((2560, 1440), (2560, 1440)),
            Effect::ViewportDestination((2560, 1440)),
            Effect::Commit,
        ],
        "live configure must request the rebuild before the first commit"
    );
}

#[test]
fn live_configure_uses_oversized_render_dimensions() {
    let mut state = ConfigureRecorder {
        session_dims: Some((1928, 1088)),
        overscan: 8,
        ..ConfigureRecorder::default()
    };

    state.reconfigure_live((1928, 1088));

    assert!(
        state
            .effects
            .contains(&Effect::Rebuild((1928, 1088), (1936, 1096))),
        "pixel shift requires a rebuild at the oversized render dimensions: {:?}",
        state.effects
    );
}
