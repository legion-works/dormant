//! HLK-LD2410 USB-serial mmWave radar sensor source.
//!
//! Parses the LD2410 periodic data frame format (factory default 256 000 baud,
//! 8N1) and emits [`PresenceEvent`]s on state transitions.
//!
//! ## Frame format (periodic data frames)
//!
//! | Offset | Size | Description |
//! |--------|------|-------------|
//! | 0      | 4    | Header `F4 F3 F2 F1` |
//! | 4      | 2    | Intra-frame length (LE u16) — data section size |
//! | 6      | 1    | Frame type: `0x02` normal, `0x01` engineering |
//! | 7      | 1    | `0xAA` head marker |
//! | 8      | 1    | Target state: `0x00` none, `0x01` moving, `0x02` stationary, `0x03` both |
//! | 9–12   | 4    | Distances/energies (ignored) |
//! | 13     | 1    | Trailer `0x55` |
//! | 14     | 1    | Check `0x00` |
//! | 15     | 4    | Tail `F8 F7 F6 F5` |
//!
//! Total frame size: 19 bytes (header 4 + length 2 + data 9 + tail 4).
//!
//! ## Fail-safe behaviour
//!
//! - Port open failure or read error → emit [`SensorState::Unavailable`] once,
//!   then retry open every 2 s (hotplug tolerant).
//! - Cancel → `Ok(())`.
//! - Raw frames arrive at ~10 Hz; the dedup layer emits [`PresenceEvent`] only
//!   when the Present/Absent state flips vs the last emitted state.

use std::time::Duration;

use async_trait::async_trait;
use dormant_core::config::schema::UsbLd2410Cfg;
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// ── Constants ──────────────────────────────────────────────────────────────────

/// Maximum internal buffer size (4 KB) to prevent runaway memory growth on
/// garbage input.
const MAX_BUF_SIZE: usize = 4096;

/// Retry interval for re-opening the serial port after a failure.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Total frame size in bytes (header 4 + length 2 + data 9 + tail 4).
#[allow(dead_code)]
const FRAME_SIZE: usize = 19;

// ── FrameParser ─────────────────────────────────────────────────────────────────

/// A pure incremental parser for LD2410 data frames.
///
/// Accumulates bytes and yields complete frames.  On garbage input it resyncs
/// by scanning for the header sequence `F4 F3 F2 F1` and discarding bytes
/// before it.  The internal buffer is capped at [`MAX_BUF_SIZE`].
#[derive(Debug, Default)]
pub struct FrameParser {
    /// Byte accumulator.
    buf: Vec<u8>,
}

/// A decoded LD2410 data frame.
///
/// Currently only `target_state` is extracted; distance/energy fields are
/// reserved for future use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ld2410Frame {
    /// Target state byte: `0x00` none, `0x01` moving, `0x02` stationary,
    /// `0x03` both.
    pub target_state: u8,
}

impl FrameParser {
    /// Create a new empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed bytes into the parser and extract any complete frames.
    ///
    /// Returns a vector of decoded frames in order.  Garbage bytes before a
    /// valid header are silently discarded.  If the internal buffer exceeds
    /// [`MAX_BUF_SIZE`] after a push, the oldest bytes are trimmed to keep it
    /// within bounds.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Ld2410Frame> {
        self.buf.extend_from_slice(bytes);

        // Cap the buffer.
        if self.buf.len() > MAX_BUF_SIZE {
            let excess = self.buf.len() - MAX_BUF_SIZE;
            self.buf.drain(..excess);
        }

        let mut frames = Vec::new();

        loop {
            // Scan for header F4 F3 F2 F1.
            let Some(header_pos) = self.find_header() else {
                // No header found — if buffer is large enough, trim oldest
                // bytes to keep it within cap.
                if self.buf.len() > MAX_BUF_SIZE / 2 {
                    let excess = self.buf.len() - MAX_BUF_SIZE / 2;
                    self.buf.drain(..excess);
                }
                break;
            };
            if header_pos > 0 {
                // Discard garbage before header.
                self.buf.drain(..header_pos);
            }

            // Need at least header (4) + length (2) = 6 bytes to read length.
            if self.buf.len() < 6 {
                break;
            }

            // Read intra-frame length (LE u16).
            let data_len = u16::from_le_bytes([self.buf[4], self.buf[5]]) as usize;

            // Bounds-check: data_len must be at least 3 to hold type + 0xAA
            // marker + target state byte.  A corrupt/short length would make
            // the field indices (6, 8) read into the tail or beyond.
            if data_len < 3 {
                // Corrupt frame — discard first byte and rescan.
                self.buf.drain(..1);
                continue;
            }

            let total_len = 4 + 2 + data_len + 4; // header + length + data + tail

            if self.buf.len() < total_len {
                // Not enough data yet.
                break;
            }

            // Validate tail F8 F7 F6 F5.
            // Invariant: total_len guarantees tail_start + 4 ≤ buf.len().
            let tail_start = 4 + 2 + data_len;
            if self.buf[tail_start..tail_start + 4] == [0xF8, 0xF7, 0xF6, 0xF5] {
                // Valid frame — extract target state.
                // data[0] = frame type, data[1] = 0xAA, data[2] = target state
                let frame_type = self.buf[6];
                let target_state = self.buf[8];

                // Only normal frames (type 0x02) carry presence data.
                // Engineering (0x01) and unknown types are ignored.
                if frame_type == 0x02 {
                    frames.push(Ld2410Frame { target_state });
                }

                // Consume the frame.
                self.buf.drain(..total_len);
            } else {
                // Bad tail — the header was likely a false positive.
                // Discard the first byte and rescan.
                self.buf.drain(..1);
            }
        }

        frames
    }

    /// Find the position of the header `F4 F3 F2 F1` in the buffer.
    /// Returns `Some(index)` or `None` if not found.
    fn find_header(&self) -> Option<usize> {
        self.buf
            .windows(4)
            .position(|w| w == [0xF4, 0xF3, 0xF2, 0xF1])
    }
}

// ── UsbLd2410Source ────────────────────────────────────────────────────────────

/// An HLK-LD2410 USB-serial mmWave radar sensor source.
pub struct UsbLd2410Source {
    /// Stable sensor identifier.
    id: SensorId,
    /// Sensor configuration.
    cfg: UsbLd2410Cfg,
}

impl UsbLd2410Source {
    /// Create a new `UsbLd2410Source`.
    #[must_use]
    pub fn new(id: SensorId, cfg: UsbLd2410Cfg) -> Self {
        Self { id, cfg }
    }

    /// Open the serial port with the configured baud rate (8N1).
    ///
    /// Returns `None` if the port cannot be opened (e.g. device not present).
    fn open_port(cfg: &UsbLd2410Cfg) -> Option<tokio_serial::SerialStream> {
        let builder = tokio_serial::new(&cfg.port, cfg.baud)
            .data_bits(tokio_serial::DataBits::Eight)
            .stop_bits(tokio_serial::StopBits::One)
            .parity(tokio_serial::Parity::None);
        match tokio_serial::SerialStream::open(&builder) {
            Ok(stream) => {
                info!(
                    "usb-ld2410: opened port '{}' at {} baud",
                    cfg.port, cfg.baud
                );
                Some(stream)
            }
            Err(e) => {
                warn!("usb-ld2410: failed to open port '{}': {e}", cfg.port,);
                None
            }
        }
    }

    /// Emit a single [`PresenceEvent`] with the given state.
    async fn emit(&self, tx: &mpsc::Sender<PresenceEvent>, state: SensorState) {
        let event = PresenceEvent::new(self.id.clone(), state, Timestamp::now());
        if tx.send(event).await.is_err() {
            // Receiver dropped — shutting down.
        }
    }
}

#[async_trait]
impl SensorSource for UsbLd2410Source {
    fn source_id(&self) -> &str {
        &self.id.0
    }

    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<PresenceEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        // ── Outer retry loop (hotplug tolerant) ────────────────────────────
        let mut unavailable_reported = false;

        loop {
            // Try to open the port.
            let Some(mut port) = Self::open_port(&self.cfg) else {
                if !unavailable_reported {
                    self.emit(&tx, SensorState::Unavailable).await;
                    unavailable_reported = true;
                }
                // Wait before retry, checking cancel.
                tokio::select! {
                    () = cancel.cancelled() => {
                        info!("usb-ld2410 '{}' cancelled", self.id);
                        return Ok(());
                    }
                    () = sleep(RETRY_INTERVAL) => {}
                }
                continue;
            };

            // Port opened — reset state.
            unavailable_reported = false;
            let mut last_emitted: Option<SensorState> = None;
            let mut parser = FrameParser::new();
            let mut read_buf = [0u8; 256];

            // ── Inner read loop ────────────────────────────────────────────
            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        info!("usb-ld2410 '{}' cancelled", self.id);
                        return Ok(());
                    }
                    result = port.read(&mut read_buf) => {
                        match result {
                            Ok(0) => {
                                // EOF (device unplugged).
                                warn!("usb-ld2410: EOF on '{}'", self.cfg.port);
                                if !unavailable_reported {
                                    self.emit(&tx, SensorState::Unavailable).await;
                                    unavailable_reported = true;
                                }
                                break; // back to outer retry loop
                            }
                            Ok(n) => {
                                let frames = parser.push(&read_buf[..n]);
                                for frame in &frames {
                                    let new_state = if frame.target_state == 0 {
                                        SensorState::Absent
                                    } else {
                                        SensorState::Present
                                    };

                                    // Dedup: only emit on flip vs last emitted.
                                    if last_emitted != Some(new_state) {
                                        debug!(
                                            "usb-ld2410: state {} -> {} (target_state=0x{:02x})",
                                            self.id,
                                            if new_state == SensorState::Present { "Present" } else { "Absent" },
                                            frame.target_state,
                                        );
                                        self.emit(&tx, new_state).await;
                                        last_emitted = Some(new_state);
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "usb-ld2410: read error on '{}': {e}",
                                    self.cfg.port,
                                );
                                if !unavailable_reported {
                                    self.emit(&tx, SensorState::Unavailable).await;
                                    unavailable_reported = true;
                                }
                                break; // back to outer retry loop
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    // ── Fixture helpers ────────────────────────────────────────────────────

    /// Build a single LD2410 normal data frame (type 0x02) with the given
    /// target state byte.
    fn make_frame(target_state: u8) -> Vec<u8> {
        let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
        let data_len: u16 = 9;
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.push(0x02); // type = normal
        buf.push(0xAA); // head marker
        buf.push(target_state);
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // distances/energies
        buf.push(0x55); // trailer
        buf.push(0x00); // check
        buf.extend_from_slice(&[0xF8, 0xF7, 0xF6, 0xF5]); // tail
        buf
    }

    /// Build an engineering frame (type 0x01) — should be ignored by parser.
    fn make_engineering_frame(target_state: u8) -> Vec<u8> {
        let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
        let data_len: u16 = 9;
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.push(0x01); // type = engineering
        buf.push(0xAA);
        buf.push(target_state);
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf.push(0x55);
        buf.push(0x00);
        buf.extend_from_slice(&[0xF8, 0xF7, 0xF6, 0xF5]);
        buf
    }

    /// Build an unknown-type frame (type 0x03) — should be ignored.
    fn make_unknown_frame(target_state: u8) -> Vec<u8> {
        let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
        let data_len: u16 = 9;
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.push(0x03); // type = unknown
        buf.push(0xAA);
        buf.push(target_state);
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf.push(0x55);
        buf.push(0x00);
        buf.extend_from_slice(&[0xF8, 0xF7, 0xF6, 0xF5]);
        buf
    }

    // ── golden_frames_decode ───────────────────────────────────────────────

    #[test]
    fn golden_frames_decode() {
        let data = include_bytes!("../../../fixtures/ld2410_frames.bin");
        let mut parser = FrameParser::new();
        let frames = parser.push(data);

        // Expected sequence from the fixture:
        //   garbage prefix (ignored)
        //   frame 1: state 0x01 (moving)
        //   frame 2: state 0x02 (stationary)
        //   frame 3: state 0x03 (both)
        //   frame 4: state 0x00 (none)
        //   frame 5: engineering 0x01 (ignored)
        //   frame 6: unknown 0x03 (ignored)
        //   frame 7: state 0x01 (moving)
        assert_eq!(frames.len(), 5, "expected 5 decoded frames (2 ignored)");

        assert_eq!(frames[0].target_state, 0x01);
        assert_eq!(frames[1].target_state, 0x02);
        assert_eq!(frames[2].target_state, 0x03);
        assert_eq!(frames[3].target_state, 0x00);
        assert_eq!(frames[4].target_state, 0x01);
    }

    // ── resync_after_garbage_prefix ────────────────────────────────────────

    #[test]
    fn resync_after_garbage_prefix() {
        let mut parser = FrameParser::new();

        // Feed garbage then a valid frame.
        let mut data = b"AAAA_BB_CCCC_DDDD_".to_vec();
        data.extend_from_slice(&make_frame(0x01));

        let frames = parser.push(&data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].target_state, 0x01);
    }

    // ── partial_frame_accumulation ─────────────────────────────────────────

    #[test]
    fn partial_frame_accumulation() {
        let mut parser = FrameParser::new();
        let frame = make_frame(0x02);

        // Split the frame into two chunks.
        let (first, second) = frame.split_at(10);

        let frames1 = parser.push(first);
        assert!(frames1.is_empty(), "no complete frame yet");

        let frames2 = parser.push(second);
        assert_eq!(frames2.len(), 1);
        assert_eq!(frames2[0].target_state, 0x02);
    }

    // ── state_dedup_only_flips_emit ────────────────────────────────────────

    #[test]
    fn state_dedup_only_flips_emit() {
        let mut parser = FrameParser::new();

        // Feed: 0x01 (Present), 0x01 (Present — dedup), 0x00 (Absent)
        let mut data = Vec::new();
        data.extend_from_slice(&make_frame(0x01));
        data.extend_from_slice(&make_frame(0x01));
        data.extend_from_slice(&make_frame(0x00));

        let frames = parser.push(&data);
        assert_eq!(frames.len(), 3);

        // Apply dedup logic (same as in run()).
        let mut last_emitted: Option<SensorState> = None;
        let mut emitted = Vec::new();

        for frame in &frames {
            let new_state = if frame.target_state == 0 {
                SensorState::Absent
            } else {
                SensorState::Present
            };
            if last_emitted != Some(new_state) {
                emitted.push(new_state);
                last_emitted = Some(new_state);
            }
        }

        assert_eq!(emitted, vec![SensorState::Present, SensorState::Absent]);
    }

    // ── accumulator_cap_prevents_growth ────────────────────────────────────

    #[test]
    fn accumulator_cap_prevents_growth() {
        let mut parser = FrameParser::new();

        // Feed 10 KB of garbage.
        let garbage = vec![0xABu8; 10_240];
        parser.push(&garbage);

        // Internal buffer should be ≤ MAX_BUF_SIZE.
        assert!(
            parser.buf.len() <= MAX_BUF_SIZE,
            "buffer grew to {} bytes (cap {})",
            parser.buf.len(),
            MAX_BUF_SIZE,
        );
    }

    // ── engineering_or_unknown_frame_types_ignored ─────────────────────────

    #[test]
    fn engineering_or_unknown_frame_types_ignored() {
        let mut parser = FrameParser::new();

        let mut data = Vec::new();
        data.extend_from_slice(&make_engineering_frame(0x01));
        data.extend_from_slice(&make_unknown_frame(0x02));
        // A normal frame after should still decode.
        data.extend_from_slice(&make_frame(0x03));

        let frames = parser.push(&data);
        assert_eq!(frames.len(), 1, "only the normal frame should decode");
        assert_eq!(frames[0].target_state, 0x03);
    }

    // ── short_frame_dropped ────────────────────────────────────────────────

    #[test]
    fn short_frame_dropped() {
        // data_len = 0, 1, 2 — all too short to hold type + marker + state.
        for short_len in 0u16..=2 {
            let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
            buf.extend_from_slice(&short_len.to_le_bytes());
            // Fill the declared data section with padding (no tail).
            buf.extend(std::iter::repeat_n(0x00, short_len as usize));
            // Append a valid frame after to verify resync works.
            buf.extend_from_slice(&make_frame(0x01));

            let mut parser = FrameParser::new();
            let frames = parser.push(&buf);
            assert_eq!(
                frames.len(),
                1,
                "data_len={short_len}: short frame should be dropped, valid frame after should decode"
            );
            assert_eq!(frames[0].target_state, 0x01);
        }
    }

    #[test]
    fn short_frame_no_panic() {
        // Fuzz-ish: data_len 0..=2 with no tail and no following frame.
        // Parser must not panic and must return no frames.
        for short_len in 0u16..=2 {
            let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
            buf.extend_from_slice(&short_len.to_le_bytes());
            buf.extend(std::iter::repeat_n(0x00, short_len as usize));

            let mut parser = FrameParser::new();
            let frames = parser.push(&buf);
            assert!(
                frames.is_empty(),
                "data_len={short_len}: no frames expected"
            );
        }
    }

    // ── unplugged_emits_unavailable ────────────────────────────────────────

    #[tokio::test]
    async fn unplugged_emits_unavailable() {
        let cfg = UsbLd2410Cfg {
            port: "/dev/nonexistent-dormant-test".into(),
            baud: 256_000,
            kind: dormant_core::config::schema::SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
        };
        let source = UsbLd2410Source::new(SensorId("test".into()), cfg);

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let cancel_spawn = cancel.clone();

        // Spawn the source.
        let handle = tokio::spawn(async move { Box::new(source).run(tx, cancel_spawn).await });

        // We should get an Unavailable event within a few seconds.
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;

        let ev = event.expect("timeout waiting for Unavailable event");
        let ev = ev.expect("channel closed before event");
        assert_eq!(ev.state, SensorState::Unavailable);
        assert_eq!(ev.sensor_id.0, "test");

        // Cancel the source.
        cancel.cancel();
        let _ = handle.await;
    }
}
