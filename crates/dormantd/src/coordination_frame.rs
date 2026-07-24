//! Bounded JSON frame transport shared by pairing and future peer coordination.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use dormant_core::peers::MAX_PAIR_FRAME_BYTES;

/// A frame could not be serialized, read, or decoded within the transport bounds.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// The peer supplied an invalid, incomplete, oversized, or malformed frame.
    InvalidFrame,
    /// Serializing a local frame failed.
    Serialize(serde_json::Error),
    /// Writing a local frame failed.
    Write {
        /// The failed write operation.
        operation: &'static str,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Serialize and write one bounded length-prefixed JSON frame.
pub(crate) async fn write_frame<T, W>(writer: &mut W, frame: &T) -> Result<(), FrameError>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(frame).map_err(FrameError::Serialize)?;
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::InvalidFrame)?;
    if !(1..=MAX_PAIR_FRAME_BYTES).contains(&length) {
        return Err(FrameError::InvalidFrame);
    }

    writer
        .write_all(&length.to_be_bytes())
        .await
        .map_err(|source| FrameError::Write {
            operation: "write frame length",
            source,
        })?;
    writer
        .write_all(&payload)
        .await
        .map_err(|source| FrameError::Write {
            operation: "write frame payload",
            source,
        })
}

/// Read and deserialize one bounded length-prefixed JSON frame.
pub(crate) async fn read_frame<T, R>(reader: &mut R) -> Result<T, FrameError>
where
    T: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let mut length_bytes = [0_u8; 4];
    reader
        .read_exact(&mut length_bytes)
        .await
        .map_err(|_| FrameError::InvalidFrame)?;
    let length = u32::from_be_bytes(length_bytes);
    if !(1..=MAX_PAIR_FRAME_BYTES).contains(&length) {
        return Err(FrameError::InvalidFrame);
    }

    let payload_len = usize::try_from(length).map_err(|_| FrameError::InvalidFrame)?;
    let mut payload = vec![0_u8; payload_len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|_| FrameError::InvalidFrame)?;
    serde_json::from_slice(&payload).map_err(|_| FrameError::InvalidFrame)
}

#[cfg(test)]
mod tests {
    use dormant_core::peers::{MAX_PAIR_FRAME_BYTES, PairFrame};

    use super::{read_frame, write_frame};

    #[tokio::test]
    async fn codec_rejects_oversized_prefix_before_allocating() {
        let bytes = (MAX_PAIR_FRAME_BYTES + 1).to_be_bytes();

        assert!(read_frame::<PairFrame, _>(&mut &bytes[..]).await.is_err());
    }

    #[tokio::test]
    async fn codec_roundtrips_json_frame() {
        let frame = PairFrame::PairResult {
            accepted: true,
            error: None,
        };
        let (mut writer, mut reader) = tokio::io::duplex(1024);

        write_frame(&mut writer, &frame).await.unwrap();
        let decoded = read_frame::<PairFrame, _>(&mut reader).await.unwrap();

        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(frame).unwrap()
        );
    }

    #[tokio::test]
    async fn codec_rejects_truncated_payload() {
        let mut bytes = Vec::from(8_u32.to_be_bytes());
        bytes.extend_from_slice(b"short");

        assert!(read_frame::<PairFrame, _>(&mut &bytes[..]).await.is_err());
    }

    #[test]
    fn legacy_identity_exchange_without_claim_port_decodes() {
        let frame: PairFrame =
            serde_json::from_str(r#"{"type":"identity_exchange","ed25519_pub":"ZmFrZS1rZXk="}"#)
                .unwrap();

        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r#"{"type":"identity_exchange","ed25519_pub":"ZmFrZS1rZXk="}"#
        );
    }

    #[test]
    fn identity_exchange_without_claim_port_keeps_legacy_encoding() {
        let frame = PairFrame::IdentityExchange {
            ed25519_pub: "ZmFrZS1rZXk=".to_owned(),
            claim_port: None,
        };

        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r#"{"type":"identity_exchange","ed25519_pub":"ZmFrZS1rZXk="}"#
        );
    }

    #[tokio::test]
    async fn identity_exchange_with_claim_port_roundtrips() {
        let frame = PairFrame::IdentityExchange {
            ed25519_pub: "ZmFrZS1rZXk=".to_owned(),
            claim_port: Some(65123),
        };
        let (mut writer, mut reader) = tokio::io::duplex(1024);

        write_frame(&mut writer, &frame).await.unwrap();
        let decoded = read_frame::<PairFrame, _>(&mut reader).await.unwrap();

        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(frame).unwrap()
        );
    }
}
