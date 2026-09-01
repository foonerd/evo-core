// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Streaming wire primitive — in-memory fan-out coordinator.
//!
//! Wraps the per-consumer mpsc channels with a multi-producer
//! / multi-consumer hub keyed on [`StreamId`]. Producers `open` a
//! stream once and `emit` frames against the resulting handle;
//! consumers `subscribe` to receive frames per their declared
//! [`BackpressurePolicy`].
//!
//! ## What this module owns
//!
//! - The [`StreamCoordinator`] hub holding registered streams +
//!   per-stream consumer lists.
//! - Per-consumer queue management (bounded mpsc with policy-
//!   driven drop semantics).
//! - Multi-consumer fan-out (single emit → N consumer pushes).
//! - `NoConsumers` / `Queued` / `Dropped` reporting back to the
//!   producer so the producer can pause production when no
//!   consumer is listening (CPU-budget discipline).
//!
//! ## What this module does not own
//!
//! - The wire-op handler that maps `subscribe_stream` requests
//!   on the wire into `StreamCoordinator::subscribe` calls. That
//!   lives in the wire layer alongside the existing
//!   `subscribe_happenings` / `subscribe_subject` handlers.
//! - The `LoadContext.streams` plugin-side handle that exposes
//!   the coordinator's `open` / `emit` / `close` operations to a
//!   plugin's verb handler. That lives in the wiring layer.
//! - Action-ledger lifecycle entries (`stream.opened`,
//!   `stream.closed`, etc.) — those wire in when the wire-op
//!   handler lands.
//!
//! ## Backpressure semantics
//!
//! The coordinator implements [`BackpressurePolicy::DropOldest`]
//! and [`BackpressurePolicy::DropNewest`]. The `Block` policy is
//! reserved for diagnostic streams (where the producer can
//! tolerate stalling); it is not implemented today and a
//! subscription requesting it returns
//! [`StreamError::UnsupportedBackpressure`] until a future
//! diagnostic-streams primitive lands a Block-aware path.
//!
//! Per-consumer queue sizing: 1 frame for `DropOldest` (latest-
//! wins) and `DropNewest` (earliest-wins). Holding a single-frame
//! queue keeps the memory footprint per consumer constant
//! regardless of producer rate, which is the right behaviour for
//! UI rendering.

use evo_plugin_sdk::contract::{
    BackpressurePolicy, EmitResult, StreamError, StreamFrame, StreamId,
    StreamSpec,
};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::Notify;

/// One-frame slot shared between the coordinator (producer side)
/// and a `ConsumerRx` (consumer side). Holds at most one frame +
/// a closed flag; the producer's policy chooses whether to
/// overwrite (DropOldest) or reject (DropNewest) when the slot is
/// full. A `Notify` wakes the consumer when a new frame lands or
/// the slot is closed.
struct ConsumerSlot {
    /// `None` = empty; `Some(frame)` = one frame waiting; `closed`
    /// flag set independently when the producer closes the
    /// stream.
    inner: Mutex<SlotState>,
    notify: Notify,
}

#[derive(Default)]
struct SlotState {
    frame: Option<StreamFrame>,
    closed: bool,
}

impl ConsumerSlot {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SlotState::default()),
            notify: Notify::new(),
        }
    }

    /// DropOldest semantics: always overwrites the slot. Returns
    /// `true` if the slot was empty (consumer was caught up),
    /// `false` if a stale frame was discarded.
    fn put_drop_oldest(&self, frame: StreamFrame) -> bool {
        let was_empty;
        {
            let mut g = self
                .inner
                .lock()
                .expect("ConsumerSlot mutex poisoned at put_drop_oldest");
            if g.closed {
                return false;
            }
            was_empty = g.frame.is_none();
            g.frame = Some(frame);
        }
        self.notify.notify_one();
        was_empty
    }

    /// DropNewest semantics: rejects when the slot is full;
    /// returns `true` when the new frame landed, `false` when it
    /// was rejected.
    fn put_drop_newest(&self, frame: StreamFrame) -> bool {
        {
            let mut g = self
                .inner
                .lock()
                .expect("ConsumerSlot mutex poisoned at put_drop_newest");
            if g.closed || g.frame.is_some() {
                return false;
            }
            g.frame = Some(frame);
        }
        self.notify.notify_one();
        true
    }

    /// Mark the slot closed. Pending frame is preserved so the
    /// consumer can drain a final `take` before seeing `None`.
    fn close(&self) {
        {
            let mut g = self
                .inner
                .lock()
                .expect("ConsumerSlot mutex poisoned at close");
            g.closed = true;
        }
        self.notify.notify_one();
    }

    /// Take the frame, awaiting if empty. Returns `None` once the
    /// slot is closed and empty.
    async fn take(&self) -> Option<StreamFrame> {
        loop {
            {
                let mut g = self
                    .inner
                    .lock()
                    .expect("ConsumerSlot mutex poisoned at take");
                if let Some(frame) = g.frame.take() {
                    return Some(frame);
                }
                if g.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    /// Try-take without awaiting. Returns `None` if the slot is
    /// empty (whether closed or not — caller can re-check).
    fn try_take(&self) -> Option<StreamFrame> {
        let mut g = self
            .inner
            .lock()
            .expect("ConsumerSlot mutex poisoned at try_take");
        g.frame.take()
    }
}

/// Handle returned by [`StreamCoordinator::open`]. The producer
/// passes this back to `emit` and `close`. Cloning the handle is
/// allowed; closing once is enough to retire the stream.
#[derive(Debug, Clone)]
pub struct StreamHandle {
    stream_id: StreamId,
}

impl StreamHandle {
    /// Borrow the stream id.
    pub fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }

    /// Construct a handle from a stream id without going through
    /// `open`. Used by SDK-trait wrappers (the in-process
    /// `CoordinatorStreamHost` and the future wire-op handler)
    /// that present a stream-id-only producer API to plugins; the
    /// underlying validation stays in the coordinator's emit /
    /// close paths, which return `StreamError::Closed` for any
    /// stream id not currently registered.
    pub fn for_id(stream_id: StreamId) -> Self {
        Self { stream_id }
    }
}

/// Receiver returned by [`StreamCoordinator::subscribe`]. Wraps
/// the per-consumer single-frame slot so the consumer awaits the
/// next frame the same way it would await any channel.
#[derive(Debug)]
pub struct ConsumerRx {
    slot: Arc<ConsumerSlot>,
    handle: ConsumerHandle,
}

impl ConsumerRx {
    /// Receive the next frame, or `None` when the stream is
    /// closed and drained.
    pub async fn recv(&mut self) -> Option<StreamFrame> {
        self.slot.take().await
    }

    /// Try to receive a frame without awaiting. Returns `None`
    /// when no frame is queued (channel may still be open).
    pub fn try_recv(&mut self) -> Option<StreamFrame> {
        self.slot.try_take()
    }

    /// Borrow the consumer handle so the consumer can call
    /// `unsubscribe` later.
    pub fn handle(&self) -> &ConsumerHandle {
        &self.handle
    }
}

impl std::fmt::Debug for ConsumerSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsumerSlot").finish()
    }
}

/// Opaque consumer registration handle. Pass this to
/// [`StreamCoordinator::unsubscribe`] to retire the consumer
/// without dropping the receiver.
#[derive(Debug, Clone)]
pub struct ConsumerHandle {
    stream_id: StreamId,
    consumer_id: u64,
}

/// In-memory fan-out coordinator. Holds the set of currently-
/// open streams + their subscribed consumers.
pub struct StreamCoordinator {
    inner: Arc<Mutex<CoordinatorInner>>,
    next_consumer_id: AtomicU64,
}

impl std::fmt::Debug for StreamCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamCoordinator").finish()
    }
}

impl Default for StreamCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct CoordinatorInner {
    /// Registered streams, keyed by `StreamId`. Each value carries
    /// the producer's spec + the per-consumer registrations.
    streams: HashMap<StreamId, StreamRegistration>,
}

struct StreamRegistration {
    spec: StreamSpec,
    consumers: Vec<ConsumerEntry>,
    /// Per-stream sequence counter slot reserved for a future
    /// stream-wide sequence (e.g., for cross-consumer ordering
    /// diagnostics). The framework's wire-protocol contract
    /// requires sequence numbers be per-connection, so the
    /// coordinator increments per-consumer below; this slot is
    /// unused today.
    #[allow(dead_code)]
    next_seq_unused: u64,
}

struct ConsumerEntry {
    consumer_id: u64,
    policy: BackpressurePolicy,
    slot: Arc<ConsumerSlot>,
    /// Per-consumer sequence counter. The wire-protocol spec
    /// requires sequence numbers be per-connection (per-consumer
    /// here, since each consumer is one connection).
    next_seq: u64,
}

impl StreamCoordinator {
    /// Construct an empty coordinator.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CoordinatorInner::default())),
            next_consumer_id: AtomicU64::new(0),
        }
    }

    /// Open (or coalesce into an existing) stream. Idempotent on
    /// matching specs; returns `StreamError::Invalid` when an
    /// existing stream's spec differs from the requested one.
    pub fn open(
        &self,
        stream_id: StreamId,
        spec: StreamSpec,
    ) -> Result<StreamHandle, StreamError> {
        let mut g = self
            .inner
            .lock()
            .expect("StreamCoordinator mutex poisoned at open");
        match g.streams.get(&stream_id) {
            Some(existing) if existing.spec == spec => {
                // Idempotent: same producer, same spec.
            }
            Some(existing) => {
                return Err(StreamError::Invalid(format!(
                    "stream {} already open with different spec; \
                     existing schema={:?}, requested schema={:?}",
                    stream_id, existing.spec.schema, spec.schema
                )));
            }
            None => {
                g.streams.insert(
                    stream_id.clone(),
                    StreamRegistration {
                        spec,
                        consumers: Vec::new(),
                        next_seq_unused: 0,
                    },
                );
            }
        }
        Ok(StreamHandle { stream_id })
    }

    /// Close a stream. Future emits against its handle return
    /// `StreamError::Closed`; subscribed consumers see their
    /// channels close on the next `recv`.
    pub fn close(&self, handle: &StreamHandle) -> Result<(), StreamError> {
        let mut g = self
            .inner
            .lock()
            .expect("StreamCoordinator mutex poisoned at close");
        let reg = g
            .streams
            .remove(&handle.stream_id)
            .ok_or_else(|| StreamError::Closed(handle.stream_id.to_string()))?;
        // Notify every consumer's slot that the stream is closed
        // so the consumer's `recv` returns `None` on the next
        // poll. The slot's queued frame (if any) survives the
        // close so the consumer can drain a final frame before
        // seeing the close signal.
        for consumer in reg.consumers.iter() {
            consumer.slot.close();
        }
        Ok(())
    }

    /// Emit one frame to every subscribed consumer. Per-consumer
    /// policy governs what happens when the consumer's queue is
    /// full. Returns the aggregate result so the producer can
    /// gauge whether to keep producing.
    pub fn emit(
        &self,
        handle: &StreamHandle,
        produced_at_ns: u64,
        codec: String,
        payload: Vec<u8>,
    ) -> Result<EmitResult, StreamError> {
        let mut g = self
            .inner
            .lock()
            .expect("StreamCoordinator mutex poisoned at emit");
        let reg = g
            .streams
            .get_mut(&handle.stream_id)
            .ok_or_else(|| StreamError::Closed(handle.stream_id.to_string()))?;
        if reg.consumers.is_empty() {
            return Ok(EmitResult::NoConsumers);
        }
        let mut queued = 0u32;
        let mut dropped = 0u32;
        for consumer in reg.consumers.iter_mut() {
            let frame = StreamFrame {
                seq: consumer.next_seq,
                produced_at_ns,
                codec: codec.clone(),
                payload: payload.clone(),
            };
            consumer.next_seq = consumer.next_seq.wrapping_add(1);
            let delivered = match consumer.policy {
                BackpressurePolicy::DropNewest => {
                    // Reject the new frame when the slot is full;
                    // older frame keeps its place.
                    consumer.slot.put_drop_newest(frame)
                }
                BackpressurePolicy::DropOldest => {
                    // Always overwrite; consumer sees the latest.
                    // The slot returns whether the prior frame was
                    // displaced; the producer treats both cases as
                    // a successful delivery (the consumer will see
                    // *some* frame).
                    consumer.slot.put_drop_oldest(frame);
                    true
                }
                BackpressurePolicy::Block => {
                    // Block is refused at the subscribe layer; this
                    // branch is unreachable from a properly-
                    // validated subscribe call. Defensive: treat as
                    // DropNewest so the producer is never blocked
                    // by a misconfigured consumer.
                    consumer.slot.put_drop_newest(frame)
                }
            };
            if delivered {
                queued += 1;
            } else {
                dropped += 1;
            }
        }
        if queued > 0 {
            Ok(EmitResult::Queued {
                consumer_count: queued,
            })
        } else {
            Ok(EmitResult::Dropped {
                consumer_count: dropped,
            })
        }
    }

    /// Subscribe a new consumer to `stream_id` with the supplied
    /// policy. Returns the receiver the consumer awaits frames
    /// on. The producer's [`StreamSpec::backpressure_policies`]
    /// gates which policies are accepted; an unsupported policy
    /// returns [`StreamError::UnsupportedBackpressure`].
    ///
    /// Subscribing to a not-yet-open stream returns
    /// `StreamError::Closed` — the producer must `open` first.
    pub fn subscribe(
        &self,
        stream_id: &StreamId,
        policy: BackpressurePolicy,
    ) -> Result<ConsumerRx, StreamError> {
        if matches!(policy, BackpressurePolicy::Block) {
            return Err(StreamError::UnsupportedBackpressure {
                stream_id: stream_id.to_string(),
                policy,
                accepted: vec![
                    BackpressurePolicy::DropOldest,
                    BackpressurePolicy::DropNewest,
                ],
            });
        }
        let consumer_id = self.next_consumer_id.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(ConsumerSlot::new());
        let mut g = self
            .inner
            .lock()
            .expect("StreamCoordinator mutex poisoned at subscribe");
        let reg = g
            .streams
            .get_mut(stream_id)
            .ok_or_else(|| StreamError::Closed(stream_id.to_string()))?;
        if !reg.spec.backpressure_policies.contains(&policy) {
            return Err(StreamError::UnsupportedBackpressure {
                stream_id: stream_id.to_string(),
                policy,
                accepted: reg.spec.backpressure_policies.clone(),
            });
        }
        reg.consumers.push(ConsumerEntry {
            consumer_id,
            policy,
            slot: Arc::clone(&slot),
            next_seq: 0,
        });
        Ok(ConsumerRx {
            slot,
            handle: ConsumerHandle {
                stream_id: stream_id.clone(),
                consumer_id,
            },
        })
    }

    /// Unsubscribe a consumer by handle. The receiver tied to the
    /// handle drops naturally; this method exists for the case
    /// where the subscriber wants to retire its registration
    /// before the receiver itself goes out of scope.
    pub fn unsubscribe(&self, handle: &ConsumerHandle) {
        let mut g = self
            .inner
            .lock()
            .expect("StreamCoordinator mutex poisoned at unsubscribe");
        if let Some(reg) = g.streams.get_mut(&handle.stream_id) {
            reg.consumers
                .retain(|c| c.consumer_id != handle.consumer_id);
        }
    }

    /// Return the current consumer count for a stream. Used by the
    /// wiring layer to surface `NoConsumers` → `FirstConsumer`
    /// transitions to the producer plugin.
    pub fn consumer_count(&self, stream_id: &StreamId) -> usize {
        let g = self
            .inner
            .lock()
            .expect("StreamCoordinator mutex poisoned at consumer_count");
        g.streams
            .get(stream_id)
            .map(|r| r.consumers.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drop_oldest_spec(schema: &str) -> StreamSpec {
        StreamSpec {
            schema: schema.into(),
            codecs: vec!["json".into()],
            max_rate_hz: 60,
            typical_payload_bytes: 256,
            backpressure_policies: vec![BackpressurePolicy::DropOldest],
        }
    }

    fn drop_newest_spec(schema: &str) -> StreamSpec {
        StreamSpec {
            schema: schema.into(),
            codecs: vec!["json".into()],
            max_rate_hz: 60,
            typical_payload_bytes: 256,
            backpressure_policies: vec![BackpressurePolicy::DropNewest],
        }
    }

    #[test]
    fn open_then_close() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("test").unwrap();
        let h = c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        assert_eq!(h.stream_id(), &id);
        c.close(&h).unwrap();
        // Second close fails (handle stale).
        assert!(matches!(c.close(&h), Err(StreamError::Closed(_))));
    }

    #[test]
    fn open_idempotent_on_matching_spec() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("test").unwrap();
        c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        c.open(id.clone(), drop_newest_spec("v1")).unwrap();
    }

    #[test]
    fn open_rejects_mismatched_spec() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("test").unwrap();
        c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        let err = c.open(id, drop_newest_spec("v2")).unwrap_err();
        assert!(matches!(err, StreamError::Invalid(_)));
    }

    #[tokio::test]
    async fn emit_with_no_consumers_returns_no_consumers() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("test").unwrap();
        let h = c.open(id, drop_newest_spec("v1")).unwrap();
        let result = c
            .emit(&h, 1000, "json".into(), b"frame-1".to_vec())
            .unwrap();
        assert_eq!(result, EmitResult::NoConsumers);
    }

    #[tokio::test]
    async fn subscribe_to_unopened_stream_fails() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("never").unwrap();
        let err = c
            .subscribe(&id, BackpressurePolicy::DropOldest)
            .unwrap_err();
        assert!(matches!(err, StreamError::Closed(_)));
    }

    #[tokio::test]
    async fn subscribe_with_unsupported_policy_refused() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("only_oldest").unwrap();
        c.open(id.clone(), drop_oldest_spec("v1")).unwrap();
        // Producer accepts only DropOldest; subscribing with
        // DropNewest fails with the typed error.
        let err = c
            .subscribe(&id, BackpressurePolicy::DropNewest)
            .unwrap_err();
        match err {
            StreamError::UnsupportedBackpressure {
                policy, accepted, ..
            } => {
                assert_eq!(policy, BackpressurePolicy::DropNewest);
                assert_eq!(accepted, vec![BackpressurePolicy::DropOldest]);
            }
            other => panic!("expected UnsupportedBackpressure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_with_block_policy_refused() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("blocky").unwrap();
        // Even when the producer's spec lists Block, this layer
        // refuses Block subscriptions because the diagnostic-
        // streams primitive that supports Block has not landed.
        let mut spec = drop_newest_spec("v1");
        spec.backpressure_policies.push(BackpressurePolicy::Block);
        c.open(id.clone(), spec).unwrap();
        let err = c.subscribe(&id, BackpressurePolicy::Block).unwrap_err();
        assert!(matches!(err, StreamError::UnsupportedBackpressure { .. }));
    }

    #[tokio::test]
    async fn single_consumer_emit_round_trip() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("rt").unwrap();
        let h = c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        let mut rx = c.subscribe(&id, BackpressurePolicy::DropNewest).unwrap();
        let result = c
            .emit(&h, 1000, "json".into(), b"frame-0".to_vec())
            .unwrap();
        assert_eq!(result, EmitResult::Queued { consumer_count: 1 });
        let frame = rx.recv().await.expect("frame received");
        assert_eq!(frame.seq, 0);
        assert_eq!(frame.produced_at_ns, 1000);
        assert_eq!(frame.codec, "json");
        assert_eq!(frame.payload, b"frame-0");

        // Second emit -> seq 1.
        c.emit(&h, 2000, "json".into(), b"frame-1".to_vec())
            .unwrap();
        let frame = rx.recv().await.expect("second frame received");
        assert_eq!(frame.seq, 1);
        assert_eq!(frame.payload, b"frame-1");
    }

    #[tokio::test]
    async fn multi_consumer_fan_out_delivers_to_all() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("fan").unwrap();
        let h = c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        let mut rx_a =
            c.subscribe(&id, BackpressurePolicy::DropNewest).unwrap();
        let mut rx_b =
            c.subscribe(&id, BackpressurePolicy::DropNewest).unwrap();
        assert_eq!(c.consumer_count(&id), 2);

        let result =
            c.emit(&h, 100, "json".into(), b"frame-x".to_vec()).unwrap();
        assert_eq!(result, EmitResult::Queued { consumer_count: 2 });
        assert_eq!(rx_a.recv().await.unwrap().payload, b"frame-x");
        assert_eq!(rx_b.recv().await.unwrap().payload, b"frame-x");
    }

    #[tokio::test]
    async fn drop_newest_policy_rejects_when_full() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("dn").unwrap();
        let h = c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        let mut rx = c.subscribe(&id, BackpressurePolicy::DropNewest).unwrap();

        // First emit fills the consumer's queue (capacity 1).
        c.emit(&h, 1, "json".into(), b"a".to_vec()).unwrap();
        // Second emit hits a full queue → DropNewest rejects → reported as Dropped.
        let result = c.emit(&h, 2, "json".into(), b"b".to_vec()).unwrap();
        assert_eq!(result, EmitResult::Dropped { consumer_count: 1 });
        // Consumer receives the FIRST frame (DropNewest preserves
        // earliest unprocessed).
        assert_eq!(rx.recv().await.unwrap().payload, b"a");
    }

    #[tokio::test]
    async fn drop_oldest_policy_keeps_latest_under_load() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("do").unwrap();
        let h = c.open(id.clone(), drop_oldest_spec("v1")).unwrap();
        let mut rx = c.subscribe(&id, BackpressurePolicy::DropOldest).unwrap();

        // Three rapid emits; consumer hasn't recv'd anything.
        c.emit(&h, 1, "json".into(), b"a".to_vec()).unwrap();
        c.emit(&h, 2, "json".into(), b"b".to_vec()).unwrap();
        c.emit(&h, 3, "json".into(), b"c".to_vec()).unwrap();

        // DropOldest semantics: receive the latest frame; older
        // ones were displaced.
        let f = rx.recv().await.unwrap();
        assert_eq!(f.payload, b"c");
        assert_eq!(f.produced_at_ns, 3);
    }

    #[tokio::test]
    async fn closing_stream_drops_consumer_channels() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("cls").unwrap();
        let h = c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        let mut rx = c.subscribe(&id, BackpressurePolicy::DropNewest).unwrap();
        c.close(&h).unwrap();
        // Consumer's channel returns None on next recv (sender
        // dropped).
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn unsubscribe_drops_consumer_from_count() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("unsub").unwrap();
        let h = c.open(id.clone(), drop_newest_spec("v1")).unwrap();
        let rx_a = c.subscribe(&id, BackpressurePolicy::DropNewest).unwrap();
        let rx_b = c.subscribe(&id, BackpressurePolicy::DropNewest).unwrap();
        assert_eq!(c.consumer_count(&id), 2);
        let handle_a = rx_a.handle().clone();
        c.unsubscribe(&handle_a);
        assert_eq!(c.consumer_count(&id), 1);
        // Emit still succeeds for the remaining consumer.
        let result = c.emit(&h, 1, "json".into(), b"x".to_vec()).unwrap();
        assert_eq!(result, EmitResult::Queued { consumer_count: 1 });
        // Drop the second receiver.
        drop(rx_b);
    }

    #[tokio::test]
    async fn emit_to_closed_stream_returns_closed() {
        let c = StreamCoordinator::new();
        let id = StreamId::new("eclosed").unwrap();
        let h = c.open(id, drop_newest_spec("v1")).unwrap();
        c.close(&h).unwrap();
        let err = c.emit(&h, 1, "json".into(), b"x".to_vec()).unwrap_err();
        assert!(matches!(err, StreamError::Closed(_)));
    }
}
