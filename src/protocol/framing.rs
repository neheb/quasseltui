//! Length-prefixed framing for the post-probe stream.
//!
//! After the probe (and TLS, if negotiated) every message is a 4-byte
//! big-endian length followed by that many payload bytes. The announced
//! length is checked against a cap *before* reading the payload so a peer
//! can't make us allocate gigabytes.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::error::{Error, Result};

pub const DEFAULT_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Prepend the 4-byte big-endian length to `payload`.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode a 4-byte big-endian length prefix.
pub fn parse_frame_header(header: &[u8]) -> Result<usize> {
    let bytes: [u8; 4] = header.try_into().map_err(|_| {
        Error::Other(format!(
            "frame header must be 4 bytes, got {}",
            header.len()
        ))
    })?;
    Ok(u32::from_be_bytes(bytes) as usize)
}

/// Read one frame: header, cap check, payload.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> Result<Vec<u8>> {
    let length = read_frame_header(reader, max_frame_bytes).await?;
    read_frame_payload(reader, length).await
}

/// Read and validate only the length prefix.
///
/// Split from the payload read so the connection can apply an idleness
/// deadline to "waiting for the next frame" and a much more generous
/// bandwidth deadline to "receiving an announced payload".
pub async fn read_frame_header<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> Result<usize> {
    let header = read_exactly(reader, 4).await?;
    let length = parse_frame_header(&header)?;
    if length > max_frame_bytes {
        return Err(Error::FrameTooLarge(format!(
            "frame length {length} exceeds max_frame_bytes {max_frame_bytes}"
        )));
    }
    Ok(length)
}

pub async fn read_frame_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
    length: usize,
) -> Result<Vec<u8>> {
    if length == 0 {
        return Ok(Vec::new());
    }
    read_exactly(reader, length).await
}

/// Write one frame and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    writer.write_all(&encode_frame(payload)).await?;
    writer.flush().await?;
    Ok(())
}

/// Read exactly `n` bytes, reporting EOF as `ConnectionClosed` with how far
/// we got.
pub(crate) async fn read_exactly<R: AsyncRead + Unpin>(
    reader: &mut R,
    n: usize,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let got = reader.read(&mut buf[filled..]).await?;
        if got == 0 {
            return Err(Error::ConnectionClosed(format!(
                "connection closed after {filled} of {n} expected bytes"
            )));
        }
        filled += got;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prepends_big_endian_length() {
        assert_eq!(encode_frame(b"hello"), b"\x00\x00\x00\x05hello");
        assert_eq!(encode_frame(b""), b"\x00\x00\x00\x00");
    }

    #[test]
    fn parse_header() {
        let header = &encode_frame(&[b'x'; 257])[..4];
        assert_eq!(parse_frame_header(header).unwrap(), 257);
        let err = parse_frame_header(b"\x00\x00").unwrap_err();
        assert!(err.to_string().contains("frame header"));
    }

    #[tokio::test]
    async fn reads_frames_in_sequence() {
        let mut data = encode_frame(b"first");
        data.extend(encode_frame(b"second"));
        data.extend(encode_frame(b""));
        let mut reader = &data[..];
        assert_eq!(
            read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES)
                .await
                .unwrap(),
            b"first"
        );
        assert_eq!(
            read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES)
                .await
                .unwrap(),
            b"second"
        );
        assert_eq!(
            read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES)
                .await
                .unwrap(),
            b""
        );
    }

    #[tokio::test]
    async fn eof_is_connection_closed() {
        let mut reader: &[u8] = b"\x00\x00";
        let err = read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ConnectionClosed(_)), "{err:?}");

        let mut reader: &[u8] = b"\x00\x00\x00\x0aabc";
        let err = read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ConnectionClosed(_)), "{err:?}");
        assert!(err.to_string().contains("after 3 of 10"));
    }

    #[tokio::test]
    async fn oversize_frame_rejected_before_payload() {
        let mut reader: &[u8] = b"\x00\x10\x00\x00";
        let err = read_frame(&mut reader, 100).await.unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge(_)));
        assert!(err.to_string().contains("exceeds max_frame_bytes"));
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let mut sink = Vec::new();
        write_frame(&mut sink, b"ping").await.unwrap();
        assert_eq!(sink, encode_frame(b"ping"));
        let mut reader = &sink[..];
        assert_eq!(
            read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES)
                .await
                .unwrap(),
            b"ping"
        );
    }
}
