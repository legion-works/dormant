//! Pure black-and-alpha renderer for the macOS tray template icon.

use dormant_core::rules::StateSnapshot;

use crate::state::{IconState, derive_icon_state};

/// Side length of the rasterized macOS template icon.
pub const TEMPLATE_PX: u32 = 36;

const BADGE_START: u32 = 23;
const BADGE_END: u32 = 35;

/// Straight-RGBA pixels for a macOS template image.
pub struct TemplatePixels {
    /// Raster width in pixels.
    pub width: u32,
    /// Raster height in pixels.
    pub height: u32,
    /// Straight RGBA pixel bytes, row-major.
    pub rgba: Vec<u8>,
}

/// Render a template icon whose status is communicated by its badge shape.
#[must_use]
pub fn render_state(state: IconState) -> TemplatePixels {
    let mut rgba = include_bytes!(concat!(env!("OUT_DIR"), "/template_mark_36.rgba")).to_vec();

    match state {
        IconState::Normal => {}
        IconState::Attention => {
            clear_badge(&mut rgba);
            draw_circle(&mut rgba, 29, 29, 4);
        }
        IconState::Paused => {
            clear_badge(&mut rgba);
            for y in 23..=32 {
                for x in 24..=26 {
                    set_black(&mut rgba, x, y);
                }
                for x in 30..=32 {
                    set_black(&mut rgba, x, y);
                }
            }
        }
        IconState::Failure => {
            clear_badge(&mut rgba);
            for y in BADGE_START..=33 {
                for x in BADGE_START..=33 {
                    if x.abs_diff(y) <= 1 || (x + y).abs_diff(56) <= 1 {
                        set_black(&mut rgba, x, y);
                    }
                }
            }
        }
        IconState::Unreachable => {
            clear_badge(&mut rgba);
            draw_ring(&mut rgba, 29, 29, 5, 3);
        }
    }

    TemplatePixels {
        width: TEMPLATE_PX,
        height: TEMPLATE_PX,
        rgba,
    }
}

/// Render a snapshot, reserving `Unreachable` for missing or disconnected IPC.
#[must_use]
pub fn render_snapshot(snapshot: Option<&StateSnapshot>, unreachable: bool) -> TemplatePixels {
    if unreachable {
        return render_state(IconState::Unreachable);
    }

    snapshot.map_or_else(
        || render_state(IconState::Unreachable),
        |snap| render_state(derive_icon_state(snap)),
    )
}

fn clear_badge(rgba: &mut [u8]) {
    for y in BADGE_START..BADGE_END {
        for x in BADGE_START..BADGE_END {
            let index = pixel_index(x, y);
            rgba[index + 3] = 0;
        }
    }
}

fn draw_circle(rgba: &mut [u8], center_x: u32, center_y: u32, radius: u32) {
    for y in BADGE_START..BADGE_END {
        for x in BADGE_START..BADGE_END {
            let dx = x.abs_diff(center_x);
            let dy = y.abs_diff(center_y);
            if dx * dx + dy * dy <= radius * radius {
                set_black(rgba, x, y);
            }
        }
    }
}

fn draw_ring(rgba: &mut [u8], center_x: u32, center_y: u32, outer_radius: u32, inner_radius: u32) {
    for y in BADGE_START..BADGE_END {
        for x in BADGE_START..BADGE_END {
            let dx = x.abs_diff(center_x);
            let dy = y.abs_diff(center_y);
            let distance_squared = dx * dx + dy * dy;
            if distance_squared <= outer_radius * outer_radius
                && distance_squared > inner_radius * inner_radius
            {
                set_black(rgba, x, y);
            }
        }
    }
}

fn set_black(rgba: &mut [u8], x: u32, y: u32) {
    let index = pixel_index(x, y);
    rgba[index..index + 4].copy_from_slice(&[0, 0, 0, 255]);
}

fn pixel_index(x: u32, y: u32) -> usize {
    usize::try_from((y * TEMPLATE_PX + x) * 4).expect("template icon index fits usize")
}

#[cfg(test)]
mod tests {
    use super::{render_snapshot, render_state};
    use crate::state::IconState;

    const BADGE_SAMPLES: &[(IconState, u32, u32)] = &[
        (IconState::Attention, 29, 29),
        (IconState::Paused, 25, 25),
        (IconState::Failure, 24, 24),
        (IconState::Unreachable, 29, 24),
    ];

    fn pixel(icon: &super::TemplatePixels, x: u32, y: u32) -> &[u8] {
        let start = usize::try_from((y * icon.width + x) * 4).expect("test pixel index fits usize");
        &icon.rgba[start..start + 4]
    }

    fn empty_snapshot() -> dormant_core::rules::StateSnapshot {
        dormant_core::rules::StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
            rollback: None,
        }
    }

    #[test]
    fn every_state_has_a_36px_rgba_buffer() {
        for state in [
            IconState::Normal,
            IconState::Attention,
            IconState::Paused,
            IconState::Failure,
            IconState::Unreachable,
        ] {
            let icon = render_state(state);
            assert_eq!(icon.width, 36);
            assert_eq!(icon.height, 36);
            assert_eq!(icon.rgba.len(), 36 * 36 * 4);
        }
    }

    #[test]
    fn every_visible_pixel_is_template_black() {
        for state in [
            IconState::Normal,
            IconState::Attention,
            IconState::Paused,
            IconState::Failure,
            IconState::Unreachable,
        ] {
            let icon = render_state(state);
            for pixel in icon.rgba.chunks_exact(4).filter(|p| p[3] != 0) {
                assert_eq!(&pixel[..3], &[0, 0, 0]);
            }
        }
    }

    #[test]
    fn states_have_distinct_shapes() {
        let states = [
            IconState::Normal,
            IconState::Attention,
            IconState::Paused,
            IconState::Failure,
            IconState::Unreachable,
        ];

        for (index, state) in states.iter().enumerate() {
            for other in &states[index + 1..] {
                assert_ne!(render_state(*state).rgba, render_state(*other).rgba);
            }
        }
    }

    #[test]
    fn render_snapshot_derives_reachable_state_and_forces_unreachable() {
        let snapshot = empty_snapshot();
        assert_eq!(
            render_snapshot(Some(&snapshot), false).rgba,
            render_state(crate::state::derive_icon_state(&snapshot)).rgba
        );
        assert_eq!(
            render_snapshot(Some(&snapshot), true).rgba,
            render_state(IconState::Unreachable).rgba
        );
        assert_eq!(
            render_snapshot(None, false).rgba,
            render_state(IconState::Unreachable).rgba
        );
    }

    #[test]
    fn badges_replace_the_base_at_their_shape_samples() {
        let normal = render_state(IconState::Normal);

        for &(state, x, y) in BADGE_SAMPLES {
            let icon = render_state(state);
            assert_eq!(pixel(&icon, x, y), &[0, 0, 0, 255]);
            assert_ne!(pixel(&normal, x, y), &[0, 0, 0, 255]);
        }
    }
}
