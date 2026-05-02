//! Wire codec: JSON / CBOR encoding and length-prefixed framing.
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
//! codec-encoded [`WireFrame`] body.
//!
//! ## Codec
//!
//! Two codecs ride the same framing layer: JSON (text, human-readable,
//! base64 byte payloads) and CBOR (binary, RFC 8949, native byte
//! strings). Both peers exchange decode-side codec lists in the
//! [`crate::wire::WireFrame::Hello`] frame; the answerer picks one
//! both can speak and echoes it on the
//! [`crate::wire::WireFrame::HelloAck`]. The Hello / HelloAck pair
//! itself is JSON-encoded for the lifetime of v1 — the codec switch
//! happens AFTER the handshake completes — so framework-side
//! handshake helpers always pass [`Codec::Json`]; callers thread the
//! negotiated [`Codec`] into their post-handshake reader / writer
//! loops.
//!
//! ## Format-aware byte fields
//!
//! [`base64_bytes`] is the serde adaptor every `Vec<u8>` payload field
//! on the wire goes through. It dispatches on the
//! `Serializer::is_human_readable()` predicate: JSON encodes the bytes
//! as a base64 string (RFC 4648 standard alphabet with padding),
//! CBOR encodes them as a native CBOR byte string. The wire-form is
//! therefore symmetric: `Vec<u8>` round-trips through either codec
//! without doubling the payload size on the binary one.
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

/// Stable wire-name for the JSON codec, as it appears in
/// [`crate::wire::WireFrame::Hello::codecs`] and
/// [`crate::wire::WireFrame::HelloAck::codec`].
pub const CODEC_NAME_JSON: &str = "json";

/// Stable wire-name for the CBOR codec.
pub const CODEC_NAME_CBOR: &str = "cbor";

/// One of the two codecs the framing layer dispatches on. Carried
/// post-handshake by the reader / writer loops on both peers.
///
/// The string forms exposed via [`Codec::name`] and parsed by
/// [`Codec::from_name`] are the on-wire identifiers and stable
/// across releases — see the wire-protocol contract in
/// `docs/engineering/PLUGIN_CONTRACT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// JSON encoding (text). Bytes serialise as base64 strings via
    /// [`base64_bytes`].
    Json,
    /// CBOR encoding (binary, RFC 8949). Bytes serialise as native
    /// CBOR byte strings via [`base64_bytes`]'s
    /// `is_human_readable`-aware branch.
    Cbor,
}

impl Codec {
    /// On-wire identifier for this codec.
    pub fn name(self) -> &'static str {
        match self {
            Self::Json => CODEC_NAME_JSON,
            Self::Cbor => CODEC_NAME_CBOR,
        }
    }

    /// Parse the on-wire identifier into a [`Codec`]. Returns `None`
    /// for any name this build does not recognise; callers map the
    /// `None` to a structured handshake refusal rather than panicking.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            CODEC_NAME_JSON => Some(Self::Json),
            CODEC_NAME_CBOR => Some(Self::Cbor),
            _ => None,
        }
    }
}

/// Format-aware serde helper for `Vec<u8>` payload fields. Encodes
/// as a base64 string when the serializer is human-readable (JSON);
/// as a native byte string otherwise (CBOR).
///
/// Apply with `#[serde(with = "crate::codec::base64_bytes")]` on
/// `Vec<u8>` fields. Without this helper serde_json encodes byte
/// sequences as arrays of integers (`[104, 101, 108, 108, 111]`) which
/// is 3-4x larger than the raw bytes and trips
/// [`MAX_FRAME_SIZE`] surprisingly early; serde_cbor / ciborium would
/// natively use CBOR byte strings without the base64 round-trip,
/// but only when the field is told to do so. The
/// `is_human_readable()` dispatch keeps both wire forms compact:
/// JSON pays the ~33% base64 tax, CBOR carries raw bytes.
///
/// Round-trip is symmetric across codecs: a `Vec<u8>` encoded by
/// either codec deserialises back to the same byte sequence under
/// the other codec, assuming the receiver uses the same helper.
/// This matters because the steward and the plugin must agree on
/// the byte-field shape regardless of which codec the handshake
/// chose.
pub mod base64_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserializer, Serializer};

    /// Serialize a byte slice. JSON path: base64 string. CBOR path:
    /// native byte string via [`Serializer::serialize_bytes`].
    pub fn serialize<S>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if s.is_human_readable() {
            s.serialize_str(&STANDARD.encode(bytes))
        } else {
            s.serialize_bytes(bytes)
        }
    }

    /// Deserialize bytes. Routes through
    /// [`Deserializer::deserialize_any`] with a visitor that
    /// accepts every reasonable wire shape: a base64 string (JSON
    /// path), a native byte string / byte buffer (CBOR path), and
    /// a `Vec<u8>`-shaped sequence as a defensive fall-back.
    ///
    /// We deliberately do NOT branch on
    /// [`Deserializer::is_human_readable`] for dispatch: ciborium's
    /// deserializer (as of 0.2) does not override the trait's
    /// default `true` return value, so callers cannot rely on the
    /// predicate to pick a wire form on the decode side. Calling
    /// `deserialize_any` lets each deserializer dispatch on the
    /// underlying value's type — strings route to `visit_str`
    /// (JSON), byte strings route to `visit_byte_buf` (CBOR) —
    /// without requiring a build-time branch we cannot trust.
    /// (`deserialize_byte_buf` is unsuitable here: serde_json
    /// invokes `visit_byte_buf` with the raw string bytes rather
    /// than `visit_str`, so the JSON path would receive the
    /// undecoded base64 string as bytes.)
    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        d.deserialize_any(BytesVisitor)
    }

    struct BytesVisitor;

    impl<'de> serde::de::Visitor<'de> for BytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str(
                "a byte sequence: base64 string (JSON) or raw bytes (CBOR)",
            )
        }

        // JSON path: serde_json sees the field as a string and
        // calls visit_str. We base64-decode it here so the JSON
        // form continues to round-trip the symmetric base64 the
        // serialize side emitted.
        fn visit_str<E: serde::de::Error>(
            self,
            v: &str,
        ) -> Result<Self::Value, E> {
            STANDARD
                .decode(v.as_bytes())
                .map_err(serde::de::Error::custom)
        }

        fn visit_string<E: serde::de::Error>(
            self,
            v: String,
        ) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        // CBOR path: ciborium routes byte strings through
        // visit_bytes (borrowed) or visit_byte_buf (owned).
        fn visit_bytes<E: serde::de::Error>(
            self,
            v: &[u8],
        ) -> Result<Self::Value, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E: serde::de::Error>(
            self,
            v: Vec<u8>,
        ) -> Result<Self::Value, E> {
            Ok(v)
        }

        // Defensive: tolerate sequence-of-integers shape too.
        // Some serde-aware encoders that don't know the field is
        // bytes deliver a `seq` of `u8`; accepting it keeps us
        // robust to encoder quirks without sacrificing the binary
        // fast path.
        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(byte) = seq.next_element::<u8>()? {
                out.push(byte);
            }
            Ok(out)
        }
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
    /// CBOR encoding failed.
    #[error("wire CBOR encode: {0}")]
    CborEncode(String),
    /// CBOR decoding failed.
    #[error("wire CBOR decode: {0}")]
    CborDecode(String),
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

/// Encode a [`WireFrame`] to CBOR bytes (no framing).
pub fn encode_cbor(frame: &WireFrame) -> Result<Vec<u8>, WireError> {
    encode_cbor_value(frame)
}

/// Decode a [`WireFrame`] from CBOR bytes (no framing).
pub fn decode_cbor(bytes: &[u8]) -> Result<WireFrame, WireError> {
    decode_cbor_value(bytes)
}

/// Generic CBOR encoder. Serialises any
/// [`serde::Serialize`] type to CBOR bytes, mapping ciborium's
/// error to [`WireError::CborEncode`].
///
/// Used by the framework for non-`WireFrame` CBOR surfaces (e.g.
/// the Fast Path channel's dedicated request / response enums)
/// without forcing each consumer to depend on ciborium directly.
pub fn encode_cbor_value<T: serde::Serialize>(
    value: &T,
) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(value, &mut out)
        .map_err(|e| WireError::CborEncode(e.to_string()))?;
    Ok(out)
}

/// Generic CBOR decoder. Deserialises any
/// [`serde::de::DeserializeOwned`] type from CBOR bytes,
/// mapping ciborium's error to [`WireError::CborDecode`].
pub fn decode_cbor_value<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, WireError> {
    ciborium::de::from_reader(bytes)
        .map_err(|e| WireError::CborDecode(e.to_string()))
}

/// Encode a [`WireFrame`] using the supplied codec (no framing).
/// Centralises the codec-dispatch arm so the framing helpers and any
/// future single-frame writers share one decision site.
pub fn encode(codec: Codec, frame: &WireFrame) -> Result<Vec<u8>, WireError> {
    match codec {
        Codec::Json => encode_json(frame),
        Codec::Cbor => encode_cbor(frame),
    }
}

/// Decode a [`WireFrame`] using the supplied codec (no framing).
pub fn decode(codec: Codec, bytes: &[u8]) -> Result<WireFrame, WireError> {
    match codec {
        Codec::Json => decode_json(bytes),
        Codec::Cbor => decode_cbor(bytes),
    }
}

/// Write one framed message to an async writer using the supplied
/// codec.
///
/// Writes the 4-byte big-endian length followed by the codec-encoded
/// payload. Fails with [`WireError::FrameTooLarge`] without writing
/// anything if the encoded payload exceeds [`MAX_FRAME_SIZE`].
///
/// The Hello / HelloAck pair must always pass [`Codec::Json`]: the
/// handshake itself is JSON-encoded for the lifetime of v1, regardless
/// of the negotiated post-handshake codec. Post-handshake reader and
/// writer loops thread the negotiated [`Codec`] through every frame.
pub async fn write_frame<W>(
    writer: &mut W,
    codec: Codec,
    frame: &WireFrame,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    let payload = encode(codec, frame)?;
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

/// Read one framed message from an async reader using the supplied
/// codec.
///
/// Reads the 4-byte big-endian length prefix, validates against
/// [`MAX_FRAME_SIZE`], then reads exactly that many payload bytes and
/// decodes them using `codec`.
///
/// Returns [`WireError::PeerClosed`] if the reader returns EOF before
/// the full length prefix arrives.
pub async fn read_frame<R>(
    reader: &mut R,
    codec: Codec,
) -> Result<WireFrame, WireError>
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
    decode(codec, &buf)
}

/// Write one framed JSON message to an async writer.
///
/// Convenience wrapper over [`write_frame`] for callers that have not
/// completed the codec handshake yet (the Hello / HelloAck pair stays
/// JSON for the lifetime of v1; see [`crate::wire::WireFrame::Hello`]
/// for the invariant).
pub async fn write_frame_json<W>(
    writer: &mut W,
    frame: &WireFrame,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, Codec::Json, frame).await
}

/// Read one framed JSON message from an async reader.
///
/// Convenience wrapper over [`read_frame`] for callers that have not
/// completed the codec handshake yet.
pub async fn read_frame_json<R>(reader: &mut R) -> Result<WireFrame, WireError>
where
    R: AsyncRead + Unpin,
{
    read_frame(reader, Codec::Json).await
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

    // ---------------------------------------------------------------
    // CBOR codec coverage. Mirrors the JSON tests above so every
    // wire shape we ship round-trips on both codecs.
    // ---------------------------------------------------------------

    #[test]
    fn encode_decode_cbor_round_trip() {
        let frame = sample_frame();
        let bytes = encode_cbor(&frame).unwrap();
        let back = decode_cbor(&bytes).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn cbor_roundtrip_includes_byte_payload_and_optional_fields() {
        // The HandleRequest variant exercises every codec-relevant
        // shape on the wire: a tagged enum (`op`), text fields,
        // an integer, a base64_bytes-flagged Vec<u8> (the CBOR
        // path encodes as a native byte string), and two
        // optional fields (one None, one Some). A round-trip
        // through the binary codec asserts the helper produces a
        // symmetric encoding.
        let frame = WireFrame::HandleRequest {
            v: PROTOCOL_VERSION,
            cid: 99,
            plugin: "org.test.x".into(),
            request_type: "echo".into(),
            payload: vec![0u8, 1, 2, b'"', b'\n', 0xFF, 0xFE],
            deadline_ms: Some(5_000),
            instance_id: Some("inst-7".into()),
        };
        let bytes = encode_cbor(&frame).unwrap();
        let back = decode_cbor(&bytes).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn cbor_byte_field_encodes_as_native_byte_string_not_base64() {
        // The whole point of format-aware base64_bytes is that the
        // CBOR wire form carries raw bytes, not a base64 string.
        // Inspect the encoded bytes for the expected major-type-2
        // (byte string) prefix `0x45` (length 5) followed by the
        // ASCII of "hello", confirming the codec did not fall
        // through to the JSON-shaped base64 path.
        let frame = WireFrame::HandleRequest {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "p".into(),
            request_type: "r".into(),
            payload: b"hello".to_vec(),
            deadline_ms: None,
            instance_id: None,
        };
        let bytes = encode_cbor(&frame).unwrap();
        // `0x45 68 65 6c 6c 6f` = byte string len 5 of "hello".
        let needle = [0x45u8, b'h', b'e', b'l', b'l', b'o'];
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "expected raw byte string in CBOR encoding; got {bytes:02x?}"
        );
        // Pinning the negative case too: the JSON-shaped base64
        // text "aGVsbG8=" must NOT appear.
        let base64_needle = b"aGVsbG8=";
        assert!(
            !bytes
                .windows(base64_needle.len())
                .any(|w| w == base64_needle),
            "CBOR encoding must not carry base64 text; got {bytes:02x?}"
        );
    }

    #[test]
    fn base64_bytes_round_trips_empty_and_binary_via_cbor() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            #[serde(with = "super::base64_bytes")]
            bytes: Vec<u8>,
        }

        // Empty byte string.
        let empty = Wrap { bytes: vec![] };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&empty, &mut buf).unwrap();
        let back: Wrap = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, empty);

        // Binary payload covering nulls, quotes, newlines, and
        // high bytes — everything that would otherwise need
        // escaping in a text encoding.
        let binary = Wrap {
            bytes: vec![0u8, 1, 2, b'"', b'\n', 255, 254],
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&binary, &mut buf).unwrap();
        let back: Wrap = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, binary);
    }

    #[tokio::test]
    async fn cbor_write_then_read_frame() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let frame = sample_frame();

        let frame_clone = frame.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut client, Codec::Cbor, &frame_clone)
                .await
                .unwrap();
            client.shutdown().await.unwrap();
        });

        let received = read_frame(&mut server, Codec::Cbor).await.unwrap();
        writer.await.unwrap();
        assert_eq!(received, frame);
    }

    #[tokio::test]
    async fn cbor_read_rejects_oversized_length_prefix() {
        let (mut client, mut server) = tokio::io::duplex(16);
        let huge = (MAX_FRAME_SIZE as u32 + 1).to_be_bytes();
        let writer = tokio::spawn(async move {
            client.write_all(&huge).await.unwrap();
            client.shutdown().await.unwrap();
        });
        let err = read_frame(&mut server, Codec::Cbor).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(err, WireError::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn cbor_read_returns_peer_closed_on_empty_stream() {
        let (client, mut server) = tokio::io::duplex(16);
        drop(client);
        let err = read_frame(&mut server, Codec::Cbor).await.unwrap_err();
        assert!(matches!(err, WireError::PeerClosed));
    }

    #[tokio::test]
    async fn cbor_read_returns_decode_error_on_invalid_payload() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let bad = b"\xff\xff\xff bogus cbor payload";
        let len = (bad.len() as u32).to_be_bytes();
        let writer = tokio::spawn(async move {
            client.write_all(&len).await.unwrap();
            client.write_all(bad).await.unwrap();
            client.shutdown().await.unwrap();
        });
        let err = read_frame(&mut server, Codec::Cbor).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(err, WireError::CborDecode(_)));
    }

    #[test]
    fn codec_name_round_trips_through_from_name() {
        assert_eq!(Codec::from_name("json"), Some(Codec::Json));
        assert_eq!(Codec::from_name("cbor"), Some(Codec::Cbor));
        assert_eq!(Codec::from_name("CBOR"), None, "case-sensitive");
        assert_eq!(Codec::from_name("protobuf"), None);
        assert_eq!(Codec::Json.name(), "json");
        assert_eq!(Codec::Cbor.name(), "cbor");
    }

    #[tokio::test]
    async fn write_frame_dispatches_on_codec_argument() {
        // Encode the same frame under both codecs through the
        // codec-aware helper and assert the resulting payloads
        // diverge — the JSON form must not equal the CBOR form
        // for any non-trivial frame. Pinning the dispatch
        // contract: a future refactor that collapses to one
        // codec silently would trip this.
        let (mut client_a, mut server_a) = tokio::io::duplex(4096);
        let (mut client_b, mut server_b) = tokio::io::duplex(4096);

        let frame = sample_frame();
        let frame_a = frame.clone();
        let frame_b = frame.clone();

        let w_a = tokio::spawn(async move {
            write_frame(&mut client_a, Codec::Json, &frame_a)
                .await
                .unwrap();
            client_a.shutdown().await.unwrap();
        });
        let w_b = tokio::spawn(async move {
            write_frame(&mut client_b, Codec::Cbor, &frame_b)
                .await
                .unwrap();
            client_b.shutdown().await.unwrap();
        });

        let mut buf_a = Vec::new();
        let mut buf_b = Vec::new();
        server_a.read_to_end(&mut buf_a).await.unwrap();
        server_b.read_to_end(&mut buf_b).await.unwrap();
        w_a.await.unwrap();
        w_b.await.unwrap();

        assert_ne!(
            buf_a, buf_b,
            "JSON and CBOR encodings must diverge on the wire"
        );
    }
}
