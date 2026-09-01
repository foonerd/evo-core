// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Streaming wire primitive — shared types.
//!
//! Continuous high-frequency ephemeral data flow distinct from
//! the existing happenings (durable, replayable, totally-ordered)
//! and subjects (state-on-change snapshot) primitives.
//!
//! ## Use cases
//!
//! - Live spectrum analyser (30-60 Hz per-channel FFT).
//! - Live VU / peak meters (60 Hz per-channel level).
//! - Long-running task progress at high update rates.
//! - Real-time sensor data (in non-audio domains).
//! - Live diagnostic streams (CPU load, memory, queue depth).
//!
//! ## Semantics
//!
//! - **Lossy by design.** Consumers tolerate gaps; rendering smooths
//!   over missed frames.
//! - **Ephemeral.** No persistence; restart drops every in-flight
//!   frame. No replay cursor.
//! - **Per-connection.** Two clients subscribed to the same
//!   `stream_id` receive independently-buffered frame sequences.
//! - **Backpressure-aware.** Per-consumer policy
//!   ([`BackpressurePolicy`]) governs what happens when the consumer
//!   is slower than the producer.
//! - **Format-negotiated.** UI client declares preferred codec /
//!   schema / rate combinations; the plugin's manifest declares
//!   what it supports; the framework picks the first match by
//!   client preference order.
//!
//! ## What this module owns
//!
//! Type definitions only — `StreamId`, `StreamSpec`, `StreamFrame`,
//! `BackpressurePolicy`, `EmitResult`, `StreamError`,
//! `FormatPreference`, `FormatOffer` — plus the pure-function
//! [`negotiate_format`] that matches a UI client's preference list
//! against a plugin's manifest-declared offers.
//!
//! ## What this module does not own
//!
//! The wire-op handler (`subscribe_stream` parsing on the wire),
//! the in-memory fan-out coordinator that holds consumer queues,
//! and the `LoadContext.streams` plugin-side handle. Those live
//! in the framework crate (the coordinator) and in the wire layer
//! (the op handler) respectively.

use serde::{Deserialize, Serialize};

/// Operator-visible stream identifier. Conventionally namespaced
/// by the producing-plugin's identity domain
/// (e.g., `audio.spectrum.com.tidal.streaming`). The framework
/// validates non-empty + no embedded null bytes; everything else
/// is the producer's namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamId(String);

impl StreamId {
    /// Construct a `StreamId` from any string-like input, validating
    /// non-empty and no embedded NUL.
    pub fn new(raw: impl Into<String>) -> Result<Self, StreamError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(StreamError::Invalid("stream_id is empty".into()));
        }
        if raw.contains('\0') {
            return Err(StreamError::Invalid(
                "stream_id contains embedded NUL".into(),
            ));
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-consumer backpressure policy. Governs what happens when the
/// consumer is slower than the producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackpressurePolicy {
    /// Drop the oldest unprocessed frame when a new frame arrives
    /// for a full queue. Producer always sees the latest state;
    /// consumer may miss intermediate frames. Default for live UI
    /// rendering.
    DropOldest,
    /// Drop the newest frame when a new frame arrives for a full
    /// queue. Consumer sees the earliest unprocessed frame;
    /// producer's newer frames are discarded.
    DropNewest,
    /// Producer is blocked until the consumer catches up.
    /// **Reserved for diagnostic streams where every frame matters
    /// and the producer can tolerate stalling.** Inappropriate
    /// for live UI rendering where stalling the producer freezes
    /// audio / sensor pipelines.
    Block,
}

impl BackpressurePolicy {
    /// Stable wire string for the policy.
    pub fn as_str(self) -> &'static str {
        match self {
            BackpressurePolicy::DropOldest => "drop_oldest",
            BackpressurePolicy::DropNewest => "drop_newest",
            BackpressurePolicy::Block => "block",
        }
    }

    /// Parse from the wire string. Returns `None` for unknown
    /// values.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "drop_oldest" => Some(BackpressurePolicy::DropOldest),
            "drop_newest" => Some(BackpressurePolicy::DropNewest),
            "block" => Some(BackpressurePolicy::Block),
            _ => None,
        }
    }
}

/// Stream specification declared at `open` time. Captures the
/// rate ceiling, the codec set the producer can encode in, and
/// the typical payload size for scheduler budgeting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSpec {
    /// Schema identifier (e.g., `"audio.spectrum.v1"`). Versioned
    /// per the framework's schema-versioning convention; bumping
    /// the schema is a manifest change that consumers detect via
    /// `format_preference`.
    pub schema: String,
    /// Codecs the producer can encode in, in preference order.
    /// Negotiated against the consumer's preferences via
    /// [`negotiate_format`].
    pub codecs: Vec<String>,
    /// Maximum frame rate the producer will emit at, in Hz.
    /// Consumers requesting lower rates downsample on the
    /// consumer side.
    pub max_rate_hz: u32,
    /// Typical encoded payload size, in bytes. Hint for scheduler
    /// budgeting; not enforced.
    pub typical_payload_bytes: u32,
    /// Backpressure policies the producer accepts. Subscriptions
    /// requesting a policy outside this set are refused with
    /// [`StreamError::UnsupportedBackpressure`].
    pub backpressure_policies: Vec<BackpressurePolicy>,
}

/// One streamed frame. Lossy, ephemeral, per-connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFrame {
    /// Per-connection sequence number assigned by the framework
    /// when the frame is fanned out to a consumer. Producer
    /// emits without a sequence; the framework numbers per
    /// consumer.
    pub seq: u64,
    /// Wall-clock nanosecond timestamp at which the producer
    /// emitted the frame.
    pub produced_at_ns: u64,
    /// Codec the framework negotiated for this consumer.
    pub codec: String,
    /// Encoded frame bytes.
    pub payload: Vec<u8>,
}

/// Result returned to the producer after `emit`. Captures whether
/// the frame reached at least one consumer queue or was dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmitResult {
    /// Frame queued for at least one consumer. Captures the
    /// consumer count at the moment of emit so the producer can
    /// stop production when none remain.
    Queued {
        /// Number of consumers that accepted the frame.
        consumer_count: u32,
    },
    /// No consumers are subscribed; frame discarded. The
    /// producer SHOULD stop computing frames in this state to
    /// save CPU.
    NoConsumers,
    /// All connected consumers' policies dropped this frame.
    /// Captures how many consumers were involved so the producer
    /// has feedback for rate-adjustment.
    Dropped {
        /// Number of consumers whose policies dropped the frame.
        consumer_count: u32,
    },
}

/// Errors raised by the streaming primitive.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StreamError {
    /// Caller supplied an invalid argument (empty stream id,
    /// embedded NUL, etc.).
    #[error("invalid stream argument: {0}")]
    Invalid(String),
    /// Caller emitted to a stream whose handle is no longer
    /// registered (closed or never opened).
    #[error("stream {0:?} is closed")]
    Closed(String),
    /// Subscription requested a backpressure policy the producer
    /// does not accept.
    #[error(
        "stream {stream_id:?} does not accept backpressure policy {policy:?}; \
         producer accepts: {accepted:?}"
    )]
    UnsupportedBackpressure {
        /// The stream id requested.
        stream_id: String,
        /// The policy the consumer asked for.
        policy: BackpressurePolicy,
        /// The set the producer declared.
        accepted: Vec<BackpressurePolicy>,
    },
    /// No codec/schema/rate combination from the consumer's
    /// `format_preference` matched the producer's offers.
    #[error(
        "no compatible format between consumer preferences and \
         producer offers; consumer wanted: {wanted:?}, producer \
         offered codecs: {offered:?}"
    )]
    IncompatibleFormat {
        /// Codecs the consumer is willing to accept.
        wanted: Vec<String>,
        /// Codecs the producer offers.
        offered: Vec<String>,
    },
}

/// One entry in a UI client's `format_preference` list. Captures
/// codec + schema + a desired delivery rate. Consumer's preference
/// order matters; the framework picks the first entry whose codec
/// + schema match the producer's offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatPreference {
    /// Codec the consumer can decode.
    pub codec: String,
    /// Schema identifier the consumer expects.
    pub schema: String,
    /// Consumer's desired delivery rate, in Hz. Producer may
    /// emit slower; never faster.
    pub rate_hz: u32,
}

/// A producer's manifest-declared offer (codec + schema). The
/// rate is bounded separately by the producer's `max_rate_hz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatOffer {
    /// Codec the producer can encode in.
    pub codec: String,
    /// Schema identifier the producer publishes.
    pub schema: String,
}

/// Negotiated format result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedFormat {
    /// The codec both parties agreed on.
    pub codec: String,
    /// The schema both parties agreed on.
    pub schema: String,
    /// The rate, capped at the lesser of consumer's `rate_hz` and
    /// producer's `max_rate_hz`.
    pub rate_hz: u32,
}

/// Pure-function format negotiation. Walks the consumer's
/// preference list in order and returns the first entry whose
/// (codec, schema) pair appears in the producer's offers, with
/// the rate clamped to the producer's `max_rate_hz`. Returns
/// `Err` when no entry matches.
pub fn negotiate_format(
    consumer_prefs: &[FormatPreference],
    producer_offers: &[FormatOffer],
    producer_max_rate_hz: u32,
) -> Result<NegotiatedFormat, StreamError> {
    for pref in consumer_prefs {
        let matched = producer_offers
            .iter()
            .any(|o| o.codec == pref.codec && o.schema == pref.schema);
        if matched {
            return Ok(NegotiatedFormat {
                codec: pref.codec.clone(),
                schema: pref.schema.clone(),
                rate_hz: pref.rate_hz.min(producer_max_rate_hz),
            });
        }
    }
    Err(StreamError::IncompatibleFormat {
        wanted: consumer_prefs.iter().map(|p| p.codec.clone()).collect(),
        offered: producer_offers.iter().map(|o| o.codec.clone()).collect(),
    })
}

/// Plugin-side handle for the streaming wire primitive. Plugins
/// receive an `Arc<dyn StreamHost>` on their [`LoadContext`] (gated
/// by the `streams` capability flag in the manifest); the handle
/// proxies producer-side operations onto the framework's
/// in-memory stream coordinator. Consumers (UI clients, other
/// plugins) reach the coordinator's subscribe surface through the
/// wire layer, not through the plugin's `LoadContext`.
///
/// ## Lifecycle
///
/// 1. The plugin calls [`StreamHost::open`] with a stable
///    [`StreamId`] and a [`StreamSpec`] declaring the schema +
///    codec + nominal rate; subsequent opens with a matching spec
///    are idempotent and the framework returns the same id.
/// 2. The plugin calls [`StreamHost::emit`] one frame at a time;
///    the framework fans the frame out to every subscribed
///    consumer per each consumer's [`BackpressurePolicy`]. The
///    [`EmitResult`] tells the producer whether anyone is
///    listening; producers are encouraged to back off when the
///    result is `NoConsumers` so a CPU-budget signal reaches the
///    plugin without forcing a poll.
/// 3. The plugin calls [`StreamHost::close`] at end-of-life or
///    when the producing source goes away. Subscribed consumers
///    see their channels close on the next `recv`.
///
/// ## Object safety + async shape
///
/// The trait is object-safe via the boxed-future shape
/// (`Pin<Box<dyn Future + Send + 'a>>`); the framework holds the
/// implementation as `Arc<dyn StreamHost>`. Async on every method
/// is the right shape because the out-of-process implementation
/// (when it lands) round-trips messages over the wire to the
/// steward; the in-process implementation completes synchronously
/// inside the framework's coordinator mutex but adopts async for
/// transport parity.
pub trait StreamHost: Send + Sync {
    /// Open (or coalesce into an existing) stream. Returns the
    /// same [`StreamId`] back on success so the plugin can chain
    /// the value through to subsequent emits without re-validating
    /// it. Idempotent on a matching spec; an existing stream with
    /// a divergent spec returns [`StreamError::Invalid`].
    fn open<'a>(
        &'a self,
        stream_id: StreamId,
        spec: StreamSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<StreamId, StreamError>>
                + Send
                + 'a,
        >,
    >;

    /// Emit one frame to every subscribed consumer. Per-consumer
    /// policy governs what happens when the consumer's queue is
    /// full; the [`EmitResult`] aggregates the fan-out for the
    /// producer's CPU-budget logic.
    fn emit<'a>(
        &'a self,
        stream_id: StreamId,
        produced_at_ns: u64,
        codec: String,
        payload: Vec<u8>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<EmitResult, StreamError>>
                + Send
                + 'a,
        >,
    >;

    /// Close a previously-opened stream. Future emits against the
    /// id return [`StreamError::Closed`]; subscribed consumers see
    /// their channels close on the next `recv`. The slot's queued
    /// frame (if any) survives the close so the consumer can drain
    /// a final frame before observing the close signal.
    fn close<'a>(
        &'a self,
        stream_id: StreamId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), StreamError>>
                + Send
                + 'a,
        >,
    >;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_id_validation() {
        assert!(StreamId::new("audio.spectrum.com.tidal").is_ok());
        assert!(matches!(
            StreamId::new("").unwrap_err(),
            StreamError::Invalid(_)
        ));
        assert!(matches!(
            StreamId::new("has\0nul").unwrap_err(),
            StreamError::Invalid(_)
        ));
    }

    #[test]
    fn backpressure_policy_round_trips_through_wire_strings() {
        for p in [
            BackpressurePolicy::DropOldest,
            BackpressurePolicy::DropNewest,
            BackpressurePolicy::Block,
        ] {
            assert_eq!(BackpressurePolicy::parse_wire(p.as_str()), Some(p));
        }
        assert_eq!(BackpressurePolicy::parse_wire("bogus"), None);
    }

    #[test]
    fn negotiate_format_picks_first_match_by_consumer_preference() {
        let prefs = vec![
            FormatPreference {
                codec: "cbor".into(),
                schema: "audio.spectrum.v1".into(),
                rate_hz: 30,
            },
            FormatPreference {
                codec: "json".into(),
                schema: "audio.spectrum.v1".into(),
                rate_hz: 30,
            },
        ];
        // Producer offers JSON only; framework picks JSON.
        let offers = vec![FormatOffer {
            codec: "json".into(),
            schema: "audio.spectrum.v1".into(),
        }];
        let n = negotiate_format(&prefs, &offers, 60).unwrap();
        assert_eq!(n.codec, "json");
        assert_eq!(n.schema, "audio.spectrum.v1");
        assert_eq!(n.rate_hz, 30);

        // Producer offers both; framework picks CBOR (consumer's
        // first preference).
        let offers = vec![
            FormatOffer {
                codec: "cbor".into(),
                schema: "audio.spectrum.v1".into(),
            },
            FormatOffer {
                codec: "json".into(),
                schema: "audio.spectrum.v1".into(),
            },
        ];
        let n = negotiate_format(&prefs, &offers, 60).unwrap();
        assert_eq!(n.codec, "cbor");
    }

    #[test]
    fn negotiate_format_clamps_rate_to_producer_max() {
        let prefs = vec![FormatPreference {
            codec: "json".into(),
            schema: "v1".into(),
            rate_hz: 240, // consumer wants 240 Hz
        }];
        let offers = vec![FormatOffer {
            codec: "json".into(),
            schema: "v1".into(),
        }];
        let n = negotiate_format(&prefs, &offers, 60).unwrap(); // producer caps 60 Hz
        assert_eq!(n.rate_hz, 60);

        // When consumer asks for less than producer max, consumer's
        // rate wins.
        let prefs = vec![FormatPreference {
            codec: "json".into(),
            schema: "v1".into(),
            rate_hz: 15,
        }];
        let n = negotiate_format(&prefs, &offers, 60).unwrap();
        assert_eq!(n.rate_hz, 15);
    }

    #[test]
    fn negotiate_format_returns_incompatible_when_no_match() {
        let prefs = vec![FormatPreference {
            codec: "cbor".into(),
            schema: "audio.spectrum.v1".into(),
            rate_hz: 30,
        }];
        let offers = vec![FormatOffer {
            codec: "json".into(),
            schema: "audio.spectrum.v1".into(),
        }];
        let err = negotiate_format(&prefs, &offers, 60).unwrap_err();
        match err {
            StreamError::IncompatibleFormat { wanted, offered } => {
                assert_eq!(wanted, vec!["cbor".to_string()]);
                assert_eq!(offered, vec!["json".to_string()]);
            }
            other => panic!("expected IncompatibleFormat, got {other:?}"),
        }
    }

    #[test]
    fn negotiate_format_requires_schema_match() {
        // Codec matches but schema does not.
        let prefs = vec![FormatPreference {
            codec: "json".into(),
            schema: "audio.spectrum.v2".into(),
            rate_hz: 30,
        }];
        let offers = vec![FormatOffer {
            codec: "json".into(),
            schema: "audio.spectrum.v1".into(),
        }];
        let err = negotiate_format(&prefs, &offers, 60).unwrap_err();
        assert!(matches!(err, StreamError::IncompatibleFormat { .. }));
    }
}
