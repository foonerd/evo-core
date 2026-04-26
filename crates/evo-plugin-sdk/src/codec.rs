//! Wire codec: JSON encoding and length-prefixed framing.
//!
//! Implements sections 6 and 9 of `docs/engineering/PLUGIN_CONTRACT.md`.
//!
//! ## Framing
//!
//! Each message on the wire is:
//!
//! ```text
//! [4-byte big-endian length] [payload]
//! ```
//!
//! The length is a `u32` in network byte order. The payload is the
//! codec-encoded [`WireFrame`](crate::wire::WireFrame) body.
//!
//! ## Codec
//!
//! v0 supports only JSON (development codec per the doc). CBOR support
//! is future work, introduced behind a `cbor` feature flag without
//! disturbing the framing layer.
//!
//! ## Size limits
//!
//! [`MAX_FRAME_SIZE`] bounds both write and read paths. Oversized
//! frames are rejected without reading the body — essential defence
//! against a misbehaving peer claiming a huge length prefix and then
//! not sending data.
//!
//! ## Async I/O
//!
//! The framing helpers operate on any type implementing
//! `tokio::io::AsyncRead` / `AsyncWrite`. Unix sockets are the
//! intended transport but nothing here is Unix-specific.

use crate::wire::WireFrame;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame payload size: 1 MiB. Matches the client-facing socket
/// server in the steward and guards against oversized frame attacks.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Serde helper for encoding `Vec<u8>` fields as base64 strings in JSON.
///
/// Apply with `#[serde(with = "crate::codec::base64_bytes")]` on
/// `Vec<u8>` fields. Without this helper serde_json encodes byte
/// sequences as arrays of integers (`[104, 101, 108, 108, 111]`) which
/// is 3-4x larger than the raw bytes and trips
/// [`MAX_FRAME_SIZE`] surprisingly early.
///
/// Uses the standard (RFC 4648) base64 alphabet with padding. The
/// output fits into JSON string literals without escaping.
///
/// Note: when a CBOR codec lands, byte fields would naturally be
/// encoded as CBOR byte strings; this helper's on-wire form becomes
/// a (still base64-encoded) text string inside the CBOR message. That
/// is suboptimal but correct; a future revision may introduce
/// format-aware encoding.
pub mod base64_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize a byte slice as a base64-encoded JSON string.
    pub fn serialize<S>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    /// Deserialize a base64-encoded JSON string into a `Vec<u8>`.
    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Wire protocol errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WireError {
    /// I/O error reading from or writing to the transport.
    #[error("wire I/O: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding or decoding failed.
    #[error("wire JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The peer advertised a frame larger than [`MAX_FRAME_SIZE`].
    #[error("frame too large: {size} bytes (limit {limit})")]
    FrameTooLarge {
        /// The size the peer advertised.
        size: usize,
        /// The configured limit.
        limit: usize,
    },
    /// The transport closed cleanly before a frame could be read.
    #[error("peer closed connection")]
    PeerClosed,
}

/// Encode a [`WireFrame`] to JSON bytes (no framing).
pub fn encode_json(frame: &WireFrame) -> Result<Vec<u8>, WireError> {
    Ok(serde_json::to_vec(frame)?)
}

/// Decode a [`WireFrame`] from JSON bytes (no framing).
pub fn decode_json(bytes: &[u8]) -> Result<WireFrame, WireError> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Write one framed JSON message to an async writer.
///
/// Writes the 4-byte big-endian length followed by the JSON payload.
/// Fails with [`WireError::FrameTooLarge`] without writing anything if
/// the encoded payload exceeds [`MAX_FRAME_SIZE`].
pub async fn write_frame_json<W>(
    writer: &mut W,
    frame: &WireFrame,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    let payload = encode_json(frame)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge {
            size: payload.len(),
            limit: MAX_FRAME_SIZE,
        });
    }
    let len_bytes = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len_bytes).await?;
    writer.write_all(&payload).await?;
    Ok(())
}

/// Read one framed JSON message from an async reader.
///
/// Reads the 4-byte big-endian length prefix, validates against
/// [`MAX_FRAME_SIZE`], then reads exactly that many payload bytes and
/// decodes them.
///
/// Returns [`WireError::PeerClosed`] if the reader returns EOF before
/// the full length prefix arrives.
pub async fn read_frame_json<R>(reader: &mut R) -> Result<WireFrame, WireError>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes).await {
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(WireError::PeerClosed);
        }
        Err(e) => return Err(WireError::Io(e)),
    }
    let size = u32::from_be_bytes(len_bytes) as usize;
    if size > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge {
            size,
            limit: MAX_FRAME_SIZE,
        });
    }
    let mut buf = vec![0u8; size];
    reader.read_exact(&mut buf).await?;
    decode_json(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_taxonomy::ErrorClass;
    use crate::wire::PROTOCOL_VERSION;
    use tokio::io::AsyncWriteExt;

    fn sample_frame() -> WireFrame {
        WireFrame::Describe {
            v: PROTOCOL_VERSION,
            cid: 42,
            plugin: "org.test.x".into(),
        }
    }

    #[test]
    fn encode_decode_json_round_trip() {
        let frame = sample_frame();
        let bytes = encode_json(&frame).unwrap();
        let back = decode_json(&bytes).unwrap();
        assert_eq!(back, frame);
    }

    #[tokio::test]
    async fn write_then_read_frame() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let frame = sample_frame();

        let frame_clone = frame.clone();
        let writer = tokio::spawn(async move {
            write_frame_json(&mut client, &frame_clone).await.unwrap();
            client.shutdown().await.unwrap();
        });

        let received = read_frame_json(&mut server).await.unwrap();
        writer.await.unwrap();
        assert_eq!(received, frame);
    }

    #[tokio::test]
    async fn write_then_read_multiple_frames() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let f1 = WireFrame::Describe {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "org.test.a".into(),
        };
        let f2 = WireFrame::Unload {
            v: PROTOCOL_VERSION,
            cid: 2,
            plugin: "org.test.a".into(),
        };
        let f3 = WireFrame::Error {
            v: PROTOCOL_VERSION,
            cid: 3,
            plugin: "org.test.a".into(),
            class: ErrorClass::ContractViolation,
            message: "nope".into(),
            details: None,
        };

        let f1c = f1.clone();
        let f2c = f2.clone();
        let f3c = f3.clone();
        let writer = tokio::spawn(async move {
            write_frame_json(&mut client, &f1c).await.unwrap();
            write_frame_json(&mut client, &f2c).await.unwrap();
            write_frame_json(&mut client, &f3c).await.unwrap();
            client.shutdown().await.unwrap();
        });

        let r1 = read_frame_json(&mut server).await.unwrap();
        let r2 = read_frame_json(&mut server).await.unwrap();
        let r3 = read_frame_json(&mut server).await.unwrap();
        writer.await.unwrap();
        assert_eq!(r1, f1);
        assert_eq!(r2, f2);
        assert_eq!(r3, f3);
    }

    #[tokio::test]
    async fn read_rejects_oversized_length_prefix() {
        let (mut client, mut server) = tokio::io::duplex(16);
        // Write a length prefix that exceeds MAX_FRAME_SIZE.
        let huge = (MAX_FRAME_SIZE as u32 + 1).to_be_bytes();
        let writer = tokio::spawn(async move {
            client.write_all(&huge).await.unwrap();
            client.shutdown().await.unwrap();
        });
        let err = read_frame_json(&mut server).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(err, WireError::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn read_returns_peer_closed_on_empty_stream() {
        let (client, mut server) = tokio::io::duplex(16);
        drop(client);
        let err = read_frame_json(&mut server).await.unwrap_err();
        assert!(matches!(err, WireError::PeerClosed));
    }

    #[tokio::test]
    async fn read_returns_io_error_on_truncated_payload() {
        let (mut client, mut server) = tokio::io::duplex(16);
        // Advertise 100 bytes, send only 4, then close.
        let writer = tokio::spawn(async move {
            client.write_all(&100u32.to_be_bytes()).await.unwrap();
            client.write_all(b"oops").await.unwrap();
            client.shutdown().await.unwrap();
        });
        let err = read_frame_json(&mut server).await.unwrap_err();
        writer.await.unwrap();
        // Truncated payload surfaces as an Io error (UnexpectedEof).
        assert!(matches!(err, WireError::Io(_)));
    }

    #[tokio::test]
    async fn read_returns_json_error_on_invalid_payload() {
        let (mut client, mut server) = tokio::io::duplex(64);
        // Valid length, invalid JSON body.
        let bad = b"not json at all";
        let len = (bad.len() as u32).to_be_bytes();
        let writer = tokio::spawn(async move {
            client.write_all(&len).await.unwrap();
            client.write_all(bad).await.unwrap();
            client.shutdown().await.unwrap();
        });
        let err = read_frame_json(&mut server).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(err, WireError::Json(_)));
    }

    #[tokio::test]
    async fn write_rejects_oversized_frame() {
        // Construct a frame whose encoded JSON exceeds MAX_FRAME_SIZE.
        // Easiest path: put a large byte payload in a ReportState.
        let big = vec![b'x'; MAX_FRAME_SIZE + 1024];
        let frame = WireFrame::ReportState {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "org.test.x".into(),
            payload: big,
            priority: crate::contract::ReportPriority::Normal,
        };

        let (mut client, _server) = tokio::io::duplex(64);
        let err = write_frame_json(&mut client, &frame).await.unwrap_err();
        assert!(matches!(err, WireError::FrameTooLarge { .. }));
    }

    #[test]
    fn max_frame_size_is_one_mib() {
        assert_eq!(MAX_FRAME_SIZE, 1024 * 1024);
    }

    #[test]
    fn base64_bytes_encodes_as_json_string_not_array() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            #[serde(with = "super::base64_bytes")]
            bytes: Vec<u8>,
        }

        let w = Wrap {
            bytes: b"hello".to_vec(),
        };
        let json = serde_json::to_string(&w).unwrap();
        // "hello" in base64 is "aGVsbG8="
        assert_eq!(json, r#"{"bytes":"aGVsbG8="}"#);
        assert!(!json.contains('['), "must not be a JSON array: {json}");

        let back: Wrap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, w);
    }

    #[test]
    fn base64_bytes_round_trips_empty_and_binary() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            #[serde(with = "super::base64_bytes")]
            bytes: Vec<u8>,
        }

        // Empty.
        let empty = Wrap { bytes: vec![] };
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(json, r#"{"bytes":""}"#);
        let back: Wrap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, empty);

        // Binary payload with bytes that would need escaping as raw
        // text: nulls, quotes, newlines.
        let binary = Wrap {
            bytes: vec![0u8, 1, 2, b'"', b'\n', 255, 254],
        };
        let json = serde_json::to_string(&binary).unwrap();
        let back: Wrap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, binary);
    }

    #[test]
    fn base64_bytes_rejects_invalid_base64() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            #[serde(with = "super::base64_bytes")]
            bytes: Vec<u8>,
        }

        let bad = r#"{"bytes":"not valid base64!!"}"#;
        let r: Result<Wrap, _> = serde_json::from_str(bad);
        assert!(r.is_err());
    }
}
