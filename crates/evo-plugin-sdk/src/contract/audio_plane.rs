// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Multi-room audio-plane SDK surface.
//!
//! Plugins admitting on the audio reference distribution's
//! multi-room shelf consume this trait to participate in the
//! framework's source-host fan-out + receiver subscription
//! plumbing. The framework owns the TCP transport, the
//! handshake, the NTP-lite sync probe, the source-host
//! election, and the per-peer connection state; plugins
//! contribute the audio-chain bridge — converting local PCM
//! into [`AudioFrameSeed`] envelopes for fan-out, and
//! consuming incoming [`AudioFrameReceived`] envelopes into
//! local audio output.
//!
//! ## Scope
//!
//! - Source-host role: plugin captures audio from its local
//!   chain (mpd-output / pcm.tee / snd-aloop / similar), chunks
//!   it into [`AudioFrameSeed`] envelopes with monotonic per-
//!   group sequence + `presentation_time_ms` derived from the
//!   framework's clock-sync, and calls
//!   [`AudioPlaneHandle::fan_out_audio_frame`] per chunk.
//! - Receiver role: plugin calls
//!   [`AudioPlaneHandle::subscribe_audio_frames`] to receive
//!   every frame the framework's transport substrate accepts;
//!   the plugin decodes and routes the payload into the local
//!   audio delivery chain at the scheduling target the
//!   envelope's `presentation_time_ms` declares.
//!
//! Role is determined by the framework's source-host election
//! (one elected source-host per multi-room group); the plugin
//! reads its role from the framework's election runtime + adapts
//! capture vs render accordingly.

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// Plugin-side handle on the framework's multi-room audio-plane
/// runtime. Admission populates
/// [`LoadContext::audio_plane`](super::context::LoadContext::audio_plane)
/// with an implementation for plugins whose manifest declares
/// `capabilities.audio_plane = true`.
pub trait AudioPlaneHandle: Send + Sync {
    /// Subscribe to the stream of every received audio frame
    /// across every connected peer.
    ///
    /// Returns an [`AudioFrameStream`] that yields each
    /// [`AudioFrameReceived`] envelope the runtime accepts.
    /// Receiver-role plugins consume this stream and decode the
    /// payload into the local audio delivery chain at the
    /// scheduling target the envelope declares.
    ///
    /// Slow consumers see [`AudioFrameStreamError::Lagged`] and
    /// rejoin at the live frame; the runtime does NOT block on
    /// subscribed plugins.
    fn subscribe_audio_frames<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        AudioFrameStream,
                        super::error::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Fan one audio frame out to every receiver of the
    /// supplied group. The framework consults the
    /// group-store + per-peer connection state and sends the
    /// envelope through every connected receiver's write-pump.
    ///
    /// Source-host-role plugins call this per encoded chunk.
    /// Failures (group unknown, no connected receivers) are
    /// non-fatal — the substrate continues to track future
    /// connections so the next call can succeed when peers
    /// come up.
    fn fan_out_audio_frame<'a>(
        &'a self,
        group_id: String,
        frame: AudioFrameSeed,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), super::error::PluginError>>
                + Send
                + 'a,
        >,
    >;

    /// Upsert a multi-room group with a caller-supplied id and
    /// member list. Idempotent: repeated calls with the same
    /// `group_id` replace the membership list without
    /// generating a fresh id. Source-host plugins call this
    /// during load to instantiate the group their TOML
    /// declares — the operator's plugin configuration is the
    /// single source of truth for group membership, and the
    /// framework's group store is the implementation.
    ///
    /// Failures (validation error, persistence error) are
    /// returned to the caller so plugin load can refuse a
    /// misconfigured group rather than silently dropping
    /// frames at fan-out time.
    fn upsert_group<'a>(
        &'a self,
        group_id: String,
        display_name: String,
        members: Vec<String>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), super::error::PluginError>>
                + Send
                + 'a,
        >,
    >;

    /// Open an outbound audio-plane connection to `addr`
    /// (host:port literal). Source-host plugins call this
    /// during load for every receiver address declared in
    /// operator config — the source-host is the active dialer,
    /// receivers stay passive listeners, so the dial direction
    /// is unambiguous and TCP-session ownership is never
    /// contested between two peers attempting to open the same
    /// connection at the same instant.
    ///
    /// Idempotent: a connection that is already established to
    /// the resolved peer is a successful no-op. Failures
    /// (DNS resolution, connect refused, handshake refused)
    /// are returned so plugin load can refuse a misconfigured
    /// member rather than fail silently. Best-effort retry on
    /// transient failures is the caller's responsibility.
    fn dial_peer<'a>(
        &'a self,
        addr: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), super::error::PluginError>>
                + Send
                + 'a,
        >,
    >;

    /// Close every outbound peer connection the runtime
    /// currently maintains. Source-role plugins call this
    /// during role-demotion teardown so the next engagement
    /// does not inherit stale connections that were dialed by
    /// the abandoned role. Inbound connections (peers that
    /// dialed THIS node) are unaffected — they belong to the
    /// remote source-host and survive local role changes.
    ///
    /// Idempotent: no outbound connections is a successful
    /// no-op. The framework sends a `Goodbye` on each
    /// connection before closing so the remote peer can drop
    /// its inbound entry cleanly rather than detecting close
    /// via read-EOF.
    fn close_outbound_connections<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), super::error::PluginError>>
                + Send
                + 'a,
        >,
    >;

    /// Subscribe to the stream of per-recipient
    /// [`FrameSendEvent`] observations the runtime emits as it
    /// queues frames onto each peer's write channel.
    ///
    /// Source-role plugins that aggregate per-frame audible-
    /// time trace records consume this stream to capture the
    /// `wire_send_ns` stage of the trace; receiver-role
    /// plugins do not need it. Failure to subscribe is
    /// non-fatal — the plugin continues with the partial
    /// trace it can observe from its own call sites.
    fn subscribe_frame_send_events<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        FrameSendEventStream,
                        super::error::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Report a per-frame trace back to the source-host of
    /// the named group. Receiver-role plugins call this once
    /// per frame they render, carrying the three receiver-
    /// side stage timestamps the framework cannot observe on
    /// the source's behalf. The framework routes the report
    /// over the audio-plane control channel to the source-
    /// host; the source-host's subscriber stream surfaces it
    /// for aggregation into the published
    /// `audio.multiroom.frame_trace` subject.
    ///
    /// Best-effort: a failed routing path (peer disconnected
    /// between frame receipt and report send) returns an
    /// error and does not retry. The next frame's report goes
    /// through if the connection re-establishes.
    fn report_frame_trace<'a>(
        &'a self,
        report: ReceiverFrameTraceReport,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), super::error::PluginError>>
                + Send
                + 'a,
        >,
    >;

    /// Subscribe to the stream of [`FrameTraceReport`] back-
    /// reports incoming from receiver peers. Source-role
    /// plugins aggregating the published `audio.multiroom.
    /// frame_trace` subject consume this stream to complete
    /// each per-frame trace record with the receiver-side
    /// stages.
    fn subscribe_frame_trace_reports<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        FrameTraceReportStream,
                        super::error::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Return the framework runtime's monotonic ns relative
    /// to its construction epoch. Plugins capturing audible-
    /// time stage timestamps use this so every same-node
    /// value is referenced to the same epoch the framework
    /// uses for [`AudioFrameReceived::wire_recv_ns`] and the
    /// per-recipient [`FrameSendEvent::wire_send_ns`] —
    /// without per-call epoch reconciliation. Cross-node
    /// reconciliation rides the audio-plane NTP-lite sync
    /// probe (the source-host aggregator carries
    /// `clock_offset_ns` alongside each trace record).
    fn monotonic_ns(&self) -> u64;

    /// The local device's canonical id. Stable for the
    /// process lifetime; matches the value the framework's
    /// `admin device identity show` wire op returns and the
    /// `from_device_id` / `source_device_id` /
    /// `receiver_device_id` fields the audio-plane stamps on
    /// every message it routes. Plugins use this to address
    /// per-device subjects (e.g. the per-device card
    /// envelope subject one source-host admission publishes
    /// per entity it cares for) and to recognise their own
    /// frames in self-loopback paths.
    fn local_device_id(&self) -> String;
}

/// Audio frame envelope a source-host plugin assembles per
/// encoded chunk and passes to
/// [`AudioPlaneHandle::fan_out_audio_frame`]. The framework
/// adds the `group_id` (passed alongside) and stamps the
/// remote view on receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFrameSeed {
    /// Per-group monotonic sequence the source-host assigns.
    /// Receivers detect drops + out-of-order arrival from this.
    pub sequence: u64,
    /// Source-host monotonic ms at which receivers should
    /// render the frame — the scheduling target the
    /// synchronised-playback alignment is built on.
    pub presentation_time_ms: u64,
    /// Codec discriminator. Current baseline: `"pcm_s16_le"`,
    /// `"pcm_s24_le"`, `"pcm_s32_le"`. Future releases add
    /// `"opus"` and other codecs without contract changes.
    pub codec: String,
    /// PCM sample rate in Hz.
    pub rate_hz: u32,
    /// Channel count.
    pub channels: u16,
    /// URL-safe base64-encoded codec payload bytes.
    pub payload_b64: String,
}

/// One audio frame as observed by the receiver-side
/// [`AudioPlaneHandle`]. Carries the on-wire envelope plus
/// the originating peer's canonical device id so subscribers
/// can correlate frames with the connection that delivered
/// them, plus the receiver-local monotonic ns at which the
/// framework decoded the frame so subscribers consuming the
/// audible-time-trace surface can correlate against the
/// source-host's per-stage timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFrameReceived {
    /// Device id of the source-host that emitted the frame.
    pub from_device_id: String,
    /// Group the frame was fanned out to.
    pub group_id: String,
    /// Per-group sequence the source-host assigned.
    pub sequence: u64,
    /// Source-host monotonic ms at which receivers should
    /// render the frame.
    pub presentation_time_ms: u64,
    /// Codec discriminator.
    pub codec: String,
    /// PCM sample rate in Hz.
    pub rate_hz: u32,
    /// Channel count.
    pub channels: u16,
    /// Decoded payload bytes (the wire base64 has been
    /// decoded once by the runtime before delivery).
    pub payload: Vec<u8>,
    /// Receiver-local monotonic ns at which the runtime
    /// finished decoding the AudioFrame envelope and queued
    /// it for delivery to subscribed plugins. Pairs with the
    /// source-host's `source_wire_send_ns` from the
    /// audible-time trace surface — together they bracket the
    /// wire-transit stage of the audio path.
    pub wire_recv_ns: u64,
}

/// One receiver's back-report of the per-frame audible-time
/// stages it observed locally. Receiver-role plugins call
/// [`AudioPlaneHandle::report_frame_trace`] with one of these
/// per frame they rendered; the framework routes the report
/// over the audio-plane control channel back to the source-
/// host of the named group, where the source-host's
/// subscriber stream surfaces it for aggregation into the
/// published `audio.multiroom.frame_trace` subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiverFrameTraceReport {
    /// Canonical device id of the source-host the frame
    /// originated from. The framework uses this to route
    /// the back-report to the correct peer.
    pub source_device_id: String,
    /// Group the frame belonged to.
    pub group_id: String,
    /// Per-group sequence the source-host assigned.
    pub sequence: u64,
    /// Receiver-local monotonic ns at which the framework
    /// decoded the AudioFrame envelope (echoed from
    /// [`AudioFrameReceived::wire_recv_ns`]).
    pub wire_recv_ns: u64,
    /// Receiver-local monotonic ns at which the receiver
    /// plugin dequeued the frame from its render scheduler
    /// for `writei`.
    pub scheduler_dequeue_ns: u64,
    /// Receiver-local monotonic ns at which the receiver-
    /// side `io.writei(&samples)` returned.
    pub writei_return_ns: u64,
}

/// One source-side observation of a frame fanned out to one
/// recipient. The framework emits one of these per frame per
/// recipient onto a broadcast channel; source-role plugins
/// subscribe via
/// [`AudioPlaneHandle::subscribe_frame_send_events`] to
/// observe the wire-send stage of the audible-time trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSendEvent {
    /// Canonical device id of the recipient the frame was
    /// queued onto.
    pub receiver_device_id: String,
    /// Group the frame belonged to.
    pub group_id: String,
    /// Per-group sequence the source-host assigned.
    pub sequence: u64,
    /// Source-host monotonic ns at which the runtime queued
    /// the frame onto this recipient's per-peer write
    /// channel.
    pub wire_send_ns: u64,
}

/// One receiver's back-reported per-frame trace observed by
/// the source-host's subscriber. The framework wraps incoming
/// [`AudioPlaneMessage::FrameTraceReport`] messages into
/// these and broadcasts them to source-role plugins that
/// subscribed via
/// [`AudioPlaneHandle::subscribe_frame_trace_reports`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTraceReport {
    /// Canonical device id of the receiver that reported the
    /// trace.
    pub from_device_id: String,
    /// Group the frame belonged to.
    pub group_id: String,
    /// Per-group sequence the source-host assigned.
    pub sequence: u64,
    /// Receiver-local monotonic ns at which the framework
    /// decoded the AudioFrame envelope.
    pub wire_recv_ns: u64,
    /// Receiver-local monotonic ns at which the receiver
    /// plugin dequeued the frame from its render scheduler.
    pub scheduler_dequeue_ns: u64,
    /// Receiver-local monotonic ns at which the receiver-
    /// side `io.writei(&samples)` returned.
    pub writei_return_ns: u64,
}

/// Stream of [`AudioFrameReceived`] events. Receiver-role
/// plugins loop on `recv()` to consume every frame the
/// framework accepts.
pub struct AudioFrameStream {
    rx: tokio::sync::broadcast::Receiver<AudioFrameReceived>,
}

impl AudioFrameStream {
    /// Framework-side constructor. Plugins receive a fully-
    /// constructed [`AudioFrameStream`] from
    /// [`AudioPlaneHandle::subscribe_audio_frames`]; this
    /// constructor is hidden so plugins cannot bypass the
    /// framework's capability gate.
    #[doc(hidden)]
    pub fn new(
        rx: tokio::sync::broadcast::Receiver<AudioFrameReceived>,
    ) -> Self {
        Self { rx }
    }

    /// Await the next received audio frame.
    pub async fn recv(
        &mut self,
    ) -> Result<AudioFrameReceived, AudioFrameStreamError> {
        match self.rx.recv().await {
            Ok(frame) => Ok(frame),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                Err(AudioFrameStreamError::Lagged { dropped: n })
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                Err(AudioFrameStreamError::Closed)
            }
        }
    }
}

/// Failure modes of [`AudioFrameStream::recv`].
#[derive(Debug, thiserror::Error)]
pub enum AudioFrameStreamError {
    /// The runtime's broadcast buffer overflowed because the
    /// subscriber consumed too slowly. Carries the count of
    /// dropped frames. The subscriber's next recv rejoins at
    /// the live frame; plugins handling this should resync
    /// their jitter buffer.
    #[error("audio-frame stream lagged: dropped {dropped} frames")]
    Lagged {
        /// Count of dropped frames between the previous recv
        /// and the lag signal.
        dropped: u64,
    },
    /// The runtime's broadcast sender was dropped. The
    /// framework is shutting down; the subscriber should
    /// terminate cleanly.
    #[error("audio-frame stream closed")]
    Closed,
}

/// Stream of [`FrameSendEvent`] observations. Source-role
/// plugins subscribed via
/// [`AudioPlaneHandle::subscribe_frame_send_events`] loop on
/// `recv()` to observe the source-side wire-send stage of the
/// audible-time trace.
pub struct FrameSendEventStream {
    rx: tokio::sync::broadcast::Receiver<FrameSendEvent>,
}

impl FrameSendEventStream {
    /// Framework-side constructor. Plugins receive a fully-
    /// constructed stream from
    /// [`AudioPlaneHandle::subscribe_frame_send_events`].
    #[doc(hidden)]
    pub fn new(rx: tokio::sync::broadcast::Receiver<FrameSendEvent>) -> Self {
        Self { rx }
    }

    /// Await the next frame-send observation.
    pub async fn recv(
        &mut self,
    ) -> Result<FrameSendEvent, AudioFrameStreamError> {
        match self.rx.recv().await {
            Ok(ev) => Ok(ev),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                Err(AudioFrameStreamError::Lagged { dropped: n })
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                Err(AudioFrameStreamError::Closed)
            }
        }
    }
}

/// Stream of [`FrameTraceReport`] back-reports from receiver
/// peers. Source-role plugins subscribed via
/// [`AudioPlaneHandle::subscribe_frame_trace_reports`] loop on
/// `recv()` to receive each receiver's per-frame stage
/// observations for aggregation into the published
/// `audio.multiroom.frame_trace` subject.
pub struct FrameTraceReportStream {
    rx: tokio::sync::broadcast::Receiver<FrameTraceReport>,
}

impl FrameTraceReportStream {
    /// Framework-side constructor.
    #[doc(hidden)]
    pub fn new(rx: tokio::sync::broadcast::Receiver<FrameTraceReport>) -> Self {
        Self { rx }
    }

    /// Await the next frame-trace back-report.
    pub async fn recv(
        &mut self,
    ) -> Result<FrameTraceReport, AudioFrameStreamError> {
        match self.rx.recv().await {
            Ok(rep) => Ok(rep),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                Err(AudioFrameStreamError::Lagged { dropped: n })
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                Err(AudioFrameStreamError::Closed)
            }
        }
    }
}
