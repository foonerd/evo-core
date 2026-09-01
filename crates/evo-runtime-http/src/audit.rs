// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Audit ledger sink: an interface the runtime mount calls
//! per [`AuditTiming`] declared on each wire op.

use crate::principal::Principal;
use async_trait::async_trait;
use evo_projection_core::{AuditTiming, WireOpId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The outcome the mount records on a per-request audit
/// event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// The dispatcher returned a successful response.
    Success,
    /// The dispatcher returned an error.
    Failure,
    /// The mount has dispatched but not yet observed the
    /// outcome (used for `OnDispatch` timing).
    Dispatched,
}

/// One audit event the mount hands to the sink.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// Which op the event is about.
    pub op_id: WireOpId,
    /// Who made the request.
    pub token_id: String,
    /// The outcome.
    pub outcome: AuditOutcome,
    /// Server-monotonic millisecond timestamp at the point
    /// the mount emitted the event.
    pub at_ms: u64,
}

/// Where the mount writes audit events. Implementations
/// must be `Send + Sync` and live for the life of the
/// listener.
///
/// Implementations should not block; the mount awaits
/// `record` on the request path, so a slow sink slows every
/// audited request. Push to a queue or fire-and-forget if
/// the underlying ledger is potentially slow.
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Record an audit event.
    async fn record(&self, event: AuditEvent);
}

/// Sink that drops every event. Useful in tests, in
/// surfaces where audit is not yet wired, and as a default
/// to keep the mount runnable without a steward ledger
/// attached.
#[derive(Debug, Default)]
pub struct NoopAuditSink {
    /// How many events have been seen. Tests use this to
    /// confirm the mount called the sink the expected
    /// number of times.
    pub seen: AtomicUsize,
}

impl NoopAuditSink {
    /// Wrap in an `Arc` for sharing with the mount.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Read the seen counter.
    pub fn seen(&self) -> usize {
        self.seen.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl AuditSink for NoopAuditSink {
    async fn record(&self, _event: AuditEvent) {
        self.seen.fetch_add(1, Ordering::Relaxed);
    }
}

/// Decide whether an event should be emitted for the given
/// (timing, outcome) pair.
pub(crate) fn timing_matches(
    timing: AuditTiming,
    outcome: AuditOutcome,
) -> bool {
    match (timing, outcome) {
        (AuditTiming::None, _) => false,
        (AuditTiming::Always, _) => true,
        (AuditTiming::OnDispatch, AuditOutcome::Dispatched) => true,
        (AuditTiming::OnSuccess, AuditOutcome::Success) => true,
        (AuditTiming::OnFailure, AuditOutcome::Failure) => true,
        _ => false,
    }
}

pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(dead_code)]
pub(crate) fn event_for(
    op_id: &WireOpId,
    principal: &Principal,
    outcome: AuditOutcome,
) -> AuditEvent {
    AuditEvent {
        op_id: op_id.clone(),
        token_id: principal.token_id.clone(),
        outcome,
        at_ms: now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_none_emits_nothing() {
        for outcome in [
            AuditOutcome::Success,
            AuditOutcome::Failure,
            AuditOutcome::Dispatched,
        ] {
            assert!(!timing_matches(AuditTiming::None, outcome));
        }
    }

    #[test]
    fn timing_always_emits_every_outcome() {
        for outcome in [
            AuditOutcome::Success,
            AuditOutcome::Failure,
            AuditOutcome::Dispatched,
        ] {
            assert!(timing_matches(AuditTiming::Always, outcome));
        }
    }

    #[test]
    fn timing_on_dispatch_emits_only_dispatch() {
        assert!(timing_matches(
            AuditTiming::OnDispatch,
            AuditOutcome::Dispatched
        ));
        assert!(!timing_matches(
            AuditTiming::OnDispatch,
            AuditOutcome::Success
        ));
        assert!(!timing_matches(
            AuditTiming::OnDispatch,
            AuditOutcome::Failure
        ));
    }

    #[test]
    fn timing_on_success_emits_only_success() {
        assert!(timing_matches(
            AuditTiming::OnSuccess,
            AuditOutcome::Success
        ));
        assert!(!timing_matches(
            AuditTiming::OnSuccess,
            AuditOutcome::Failure
        ));
        assert!(!timing_matches(
            AuditTiming::OnSuccess,
            AuditOutcome::Dispatched
        ));
    }

    #[test]
    fn timing_on_failure_emits_only_failure() {
        assert!(timing_matches(
            AuditTiming::OnFailure,
            AuditOutcome::Failure
        ));
        assert!(!timing_matches(
            AuditTiming::OnFailure,
            AuditOutcome::Success
        ));
        assert!(!timing_matches(
            AuditTiming::OnFailure,
            AuditOutcome::Dispatched
        ));
    }

    #[tokio::test]
    async fn noop_sink_counts_calls() {
        let sink = NoopAuditSink::shared();
        let op = WireOpId::new("describe_capabilities").unwrap();
        for _ in 0..3 {
            sink.record(AuditEvent {
                op_id: op.clone(),
                token_id: "tok".into(),
                outcome: AuditOutcome::Success,
                at_ms: 0,
            })
            .await;
        }
        assert_eq!(sink.seen(), 3);
    }
}
