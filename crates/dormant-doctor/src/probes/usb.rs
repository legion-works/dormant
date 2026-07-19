//! USB LD2410 radar sensor probe — opens a serial port and decodes frames.
//!
//! ## Platform coverage
//!
//! The production probe supports Linux and macOS. Its PTY integration test
//! is Linux-only: serialport-rs applies macOS's `IOSSIOSPEED` ioctl for every
//! baud rate, while macOS PTYs reject that ioctl with `ENOTTY` (serialport-rs
//! issue #22). On macOS 26.5 this was observed with both 256000 and 115200;
//! the resulting early slave close then makes the master writer report EIO.
//! Real serial devices support `IOSSIOSPEED`, so macOS coverage of the open
//! path belongs to on-target doctor acceptance rather than this PTY fake.

use crate::types::ProbeResult;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use dormant_core::types::SensorState;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use dormant_sensors::usb_ld2410::FrameParser;

/// Probe a USB LD2410 sensor on the given serial port.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn probe_usb(port: &str, baud: u32) -> ProbeResult {
    probe_usb_with_observer(port, baud, None).await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn probe_usb_with_observer(
    port: &str,
    baud: u32,
    opened_observer: Option<tokio::sync::oneshot::Sender<()>>,
) -> ProbeResult {
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

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
    if let Some(observer) = opened_observer {
        let _ = observer.send(());
    }

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

/// USB serial probing is only supported on Linux and macOS in this release.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub async fn probe_usb(_port: &str, _baud: u32) -> ProbeResult {
    ProbeResult::not_supported(
        format!("usb {_port}"),
        "USB serial is only supported on Linux and macOS in this release",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::types::ProbeStatus;
    #[cfg(target_os = "linux")]
    use tokio::sync::oneshot;

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

    /// Build a single LD2410 "present" (moving) data frame — byte-for-byte
    /// the same layout `usb_frame_parser_decodes_present` above builds
    /// inline, and the same layout `dormant_sensors::usb_ld2410`'s own
    /// private `#[cfg(test)]` `make_frame` helper builds (that helper is
    /// `fn`, not `pub`, scoped to that crate's own test module, so it
    /// cannot be imported from here — this reproduces the documented frame
    /// format table at the top of `dormant_sensors::usb_ld2410` instead of
    /// hand-rolling an unrelated byte layout).
    #[cfg(target_os = "linux")]
    fn ld2410_present_frame() -> Vec<u8> {
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
        buf
    }

    // ── TestPty — a real Linux PTY pair, proving `probe_usb`'s serial I/O
    // path end to end (this is NOT a fake: `tokio_serial::SerialStream`
    // opens the slave path exactly as it would open a real
    // `/dev/ttyUSB0`). This harness is intentionally Linux-only: macOS PTYs
    // reject serialport-rs's unconditional IOSSIOSPEED ioctl with ENOTTY
    // (serialport-rs#22), unlike real macOS serial devices.

    #[cfg(target_os = "linux")]
    struct TestPty {
        master: std::fs::File,
        slave_path: std::path::PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl TestPty {
        fn open() -> std::io::Result<Self> {
            use std::os::unix::io::FromRawFd;

            let mut master_fd: libc::c_int = -1;
            let mut slave_fd: libc::c_int = -1;
            let mut name_buf: [libc::c_char; 64] = [0; 64];

            // Safety: all five pointers are either valid out-params
            // (`master_fd`, `slave_fd`, `name_buf`) sized per `openpty`'s
            // contract, or explicit nulls for the optional `termp`/`winp`
            // arguments (both documented as acceptable when the caller
            // wants the pty's default terminal attributes/window size).
            let rc = unsafe {
                libc::openpty(
                    &raw mut master_fd,
                    &raw mut slave_fd,
                    name_buf.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // Safety: `slave_fd` is a valid, open fd `openpty` just
            // returned. The probe opens this path independently via
            // `tokio_serial`, so the test's setup copy is no longer needed.
            unsafe { libc::close(slave_fd) };

            // Safety: `openpty` NUL-terminates `name_buf` on success.
            let path_cstr = unsafe { std::ffi::CStr::from_ptr(name_buf.as_ptr()) };
            let slave_path = std::path::PathBuf::from(path_cstr.to_string_lossy().into_owned());

            // Safety: `master_fd` is a valid, open, uniquely-owned fd from
            // `openpty` above; wrapping it in a `File` gives us
            // `Write`/`Drop`-based cleanup with no further raw-fd handling.
            let master = unsafe { std::fs::File::from_raw_fd(master_fd) };

            Ok(Self { master, slave_path })
        }

        fn slave_path(&self) -> &std::path::Path {
            &self.slave_path
        }

        /// Duplicate the master fd into an independent `File` — used so a
        /// writer thread can hold its own handle while `self` (and the
        /// original master fd) stays alive in the test's own scope for the
        /// full duration of the probe call. The PTY slave sees EOF the
        /// moment the LAST open master fd closes; `probe_usb` treats EOF as
        /// an unconditional `Fail` (device disconnected) regardless of how
        /// many frames were already decoded, so this test must keep some
        /// master fd open until after `probe_usb` returns.
        fn try_clone_master(&self) -> std::io::Result<std::fs::File> {
            use std::os::unix::io::{AsRawFd, FromRawFd};

            // Safety: `self.master`'s fd is valid for the lifetime of
            // `self`; `dup` returns a new, independently-closable fd
            // referring to the same open file description.
            let dup_fd = unsafe { libc::dup(self.master.as_raw_fd()) };
            if dup_fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Safety: `dup_fd` was just returned by a successful `dup`
            // call above and is not owned anywhere else yet.
            Ok(unsafe { std::fs::File::from_raw_fd(dup_fd) })
        }
    }

    /// RED-FIRST (Task 11): before this test and `TestPty` existed,
    /// `probe_usb`'s serial I/O path had never been proven against a real
    /// byte stream in this sandbox — only the pure `FrameParser` logic
    /// above was. This test opens a real Unix PTY pair, writes one LD2410
    /// "present" frame to the master from a background thread, and asserts
    /// `probe_usb` — reading the SLAVE path through the exact same
    /// `tokio_serial::SerialStream::open` + `FrameParser` code path
    /// production uses — decodes it correctly. macOS PTYs are deliberately
    /// excluded because serialport-rs always applies IOSSIOSPEED there and
    /// PTYs reject it with ENOTTY; real-device validation covers that path.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn usb_probe_decodes_a_frame_over_a_unix_pty() {
        let pty = TestPty::open().expect("openpty");
        let slave_path = pty.slave_path().to_string_lossy().into_owned();

        let mut writer = pty
            .try_clone_master()
            .expect("dup master for writer thread");
        let frame = ld2410_present_frame();
        let (opened_tx, opened_rx) = oneshot::channel();
        let probe_path = slave_path.clone();
        let probe = tokio::spawn(async move {
            probe_usb_with_observer(&probe_path, 256_000, Some(opened_tx)).await
        });
        opened_rx.await.expect("serial stream opened");
        std::io::Write::write_all(&mut writer, &frame).expect("write frame to pty master");

        // `pty` (holding the ORIGINAL master fd) stays alive in this scope
        // for the whole `probe_usb` call below — see `try_clone_master`'s
        // docs for why an early close would flip a decoded Pass into a
        // spurious EOF Fail.
        let result = probe.await.expect("probe task panicked");

        drop(pty);

        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(
            result.detail.contains("last state: present"),
            "detail should report last state: present; got: {}",
            result.detail
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn nonexistent_port_returns_structured_failure() {
        let result = probe_usb("/dev/dormant-does-not-exist", 256_000).await;

        assert_eq!(result.status, crate::types::ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("failed to open port"), "{result:?}");
    }
}
