#![cfg(target_os = "linux")]

use dormant_core::config::schema::StreamMode;
use dormantd::active_sampler::{CaptureSource, DisplayExpectation, linux::PortalPipeWireSource};

#[tokio::test]
#[ignore = "requires an operator-present portal consent flow"]
async fn portal_pipewire_live_reduces_one_transient_frame() {
    assert_eq!(
        std::env::var("DORMANT_RUN_PORTAL_TESTS").as_deref(),
        Ok("1"),
        "set DORMANT_RUN_PORTAL_TESTS=1 before invoking this test"
    );
    let mut source = PortalPipeWireSource::new()
        .await
        .expect("connect to the ScreenCast portal");
    source
        .request_consent(&DisplayExpectation {
            display: "operator-selected".to_owned(),
            compositor_output: None,
        })
        .await
        .expect("grant monitor consent");
    let frame = source
        .capture_one(StreamMode::Warm)
        .await
        .expect("capture a portal frame");
    let grid = dormant_core::spatial_grid::reduce_rgba8_to_luma_grid(
        &frame.rgba,
        frame.width,
        frame.height,
        frame.stride,
        16,
        9,
    )
    .expect("reduce frame to 16x9 luma");
    assert_eq!(grid.cells.len(), 16 * 9);
    source.close().await;
}
