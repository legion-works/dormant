//! USB LD2410 radar sensor probe — opens a serial port and decodes frames.

use crate::types::ProbeResult;
use dormant_core::types::SensorState;
use dormant_sensors::usb_ld2410::FrameParser;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Probe a USB LD2410 sensor on the given serial port.
pub async fn probe_usb(port: &str, baud: u32) -> ProbeResult {
    let builder = tokio_serial::new(port, baud)
        .data_bits(tokio_serial::DataBits::Eight)
        .stop_bits(tokio_serial::StopBits::One)
        .parity(tokio_serial::Parity::None);

    let mut stream = match tokio_serial::SerialStream::open(&builder) {
        Ok(s) => s,
        Err(e) => {
            return ProbeResult::fail(format!("usb {port}"), format!("failed to open port: {e}"));
        }
    };

    let mut parser = FrameParser::new();
    let mut buf = [0u8; 256];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut total_frames = 0usize;
    let mut last_state: Option<SensorState> = None;

    while tokio::time::Instant::now() < deadline {
        let timeout = deadline - tokio::time::Instant::now();
        let result = tokio::time::timeout(timeout, stream.read(&mut buf)).await;

        match result {
            Ok(Ok(0)) => {
                return ProbeResult::fail(
                    format!("usb {port}"),
                    "port returned EOF (device disconnected)".to_string(),
                );
            }
            Ok(Ok(n)) => {
                let frames = parser.push(&buf[..n]);
                total_frames += frames.len();
                for frame in frames {
                    let state = if frame.target_state == 0 {
                        SensorState::Absent
                    } else {
                        SensorState::Present
                    };
                    last_state = Some(state);
                }
            }
            Ok(Err(e)) => {
                return ProbeResult::fail(format!("usb {port}"), format!("read error: {e}"));
            }
            Err(_elapsed) => break, // timeout
        }
    }

    if total_frames == 0 {
        return ProbeResult::fail(
            format!("usb {port}"),
            "port opened but no LD2410 frames decoded (wrong port? wrong baud?)".to_string(),
        );
    }

    let state_str = match last_state {
        Some(SensorState::Present) => "present",
        Some(SensorState::Absent) => "absent",
        Some(SensorState::Unavailable) => "unavailable",
        None => "unknown",
    };

    ProbeResult::pass(
        format!("usb {port}"),
        format!("{total_frames} frames decoded, last state: {state_str}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_frame_parser_decodes_present() {
        let mut parser = FrameParser::new();
        let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
        let data_len: u16 = 9;
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.push(0x02); // type = normal
        buf.push(0xAA); // head marker
        buf.push(0x01); // target_state = moving (present)
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf.push(0x55);
        buf.push(0x00);
        buf.extend_from_slice(&[0xF8, 0xF7, 0xF6, 0xF5]);

        let frames = parser.push(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].target_state, 0x01);
    }

    #[test]
    fn usb_frame_parser_decodes_absent() {
        let mut parser = FrameParser::new();
        let mut buf = vec![0xF4, 0xF3, 0xF2, 0xF1];
        let data_len: u16 = 9;
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.push(0x02);
        buf.push(0xAA);
        buf.push(0x00); // target_state = none (absent)
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf.push(0x55);
        buf.push(0x00);
        buf.extend_from_slice(&[0xF8, 0xF7, 0xF6, 0xF5]);

        let frames = parser.push(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].target_state, 0x00);
    }
}
