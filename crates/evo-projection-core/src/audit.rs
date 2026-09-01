// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Audit-emission timing for wire operations.
//!
//! Every wire operation declares when it emits to the audit
//! ledger. Concrete projections consume the declaration uniformly:
//! the same op emits at the same point on REST, WebSocket, gRPC,
//! GraphQL, and HTTP/3.
//!
//! The audit ledger itself (the durable persistence layer + the
//! `LifecycleEventType` payload taxonomy) lives in the framework's
//! ledger primitive. This crate only declares the *timing*; the
//! event-type taxonomy and payload shapes are framework concerns
//! consumed by the projection layer when it composes the actual
//! emit-call.

use serde::{Deserialize, Serialize};

/// When a wire operation emits to the audit ledger.
///
/// The variants distinguish the operationally meaningful timing
/// points. Most read-only discovery ops use [`Self::None`]; most
/// state-changing ops use [`Self::Always`] (both success and
/// failure are recorded so audit-trail consumers can reconstruct
/// the full dispatch history). [`Self::OnDispatch`] is for ops
/// where the *attempt* is the auditable event regardless of
/// outcome (long-running mutations whose body may not return for
/// minutes). [`Self::OnSuccess`] and [`Self::OnFailure`] cover
/// the narrower cases where only one outcome warrants a ledger
/// entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditTiming {
    /// No audit emission for this op. Reserved for read-only
    /// discovery (capability listing, schema introspection)
    /// where the dispatch produces no operator-relevant state
    /// change.
    None,

    /// Emit on dispatch entry, before the op body runs. Used
    /// for long-running mutations whose attempt is the
    /// auditable event regardless of body outcome (steward
    /// restart, plugin install, update apply — the operator
    /// intent is recorded the moment dispatch begins).
    OnDispatch,

    /// Emit on successful completion only. Used for mutations
    /// where a failed attempt produces no operator-relevant
    /// state change (admission policy refusal returns the error
    /// to the caller; no need to ledger the refusal because the
    /// state did not change).
    OnSuccess,

    /// Emit on failure only. Used for ops where the success
    /// path is the silent default and the failure path is the
    /// alarming event (capability denial, step-up auth failure,
    /// policy refusal — failures alone surface to operators).
    OnFailure,

    /// Emit on both success and failure paths. Used for ops
    /// where every dispatch attempt warrants a ledger entry
    /// regardless of outcome — privileged operations under
    /// step-up auth, plugin lifecycle transitions, channel
    /// changes, capability grants and revocations.
    Always,
}

impl AuditTiming {
    /// Whether this timing emits on the dispatch-entry hook.
    pub fn emits_on_dispatch(&self) -> bool {
        matches!(self, Self::OnDispatch)
    }

    /// Whether this timing emits on a successful body return.
    pub fn emits_on_success(&self) -> bool {
        matches!(self, Self::OnSuccess | Self::Always)
    }

    /// Whether this timing emits on a failed body return.
    pub fn emits_on_failure(&self) -> bool {
        matches!(self, Self::OnFailure | Self::Always)
    }

    /// Whether this timing produces any ledger entry across the
    /// full dispatch lifecycle. `None` returns `false`; every
    /// other variant returns `true`.
    pub fn produces_any_entry(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_produces_no_entry() {
        let t = AuditTiming::None;
        assert!(!t.produces_any_entry());
        assert!(!t.emits_on_dispatch());
        assert!(!t.emits_on_success());
        assert!(!t.emits_on_failure());
    }

    #[test]
    fn on_dispatch_emits_at_entry_only() {
        let t = AuditTiming::OnDispatch;
        assert!(t.produces_any_entry());
        assert!(t.emits_on_dispatch());
        assert!(!t.emits_on_success());
        assert!(!t.emits_on_failure());
    }

    #[test]
    fn on_success_emits_on_success_only() {
        let t = AuditTiming::OnSuccess;
        assert!(t.produces_any_entry());
        assert!(!t.emits_on_dispatch());
        assert!(t.emits_on_success());
        assert!(!t.emits_on_failure());
    }

    #[test]
    fn on_failure_emits_on_failure_only() {
        let t = AuditTiming::OnFailure;
        assert!(t.produces_any_entry());
        assert!(!t.emits_on_dispatch());
        assert!(!t.emits_on_success());
        assert!(t.emits_on_failure());
    }

    #[test]
    fn always_emits_on_both_success_and_failure() {
        let t = AuditTiming::Always;
        assert!(t.produces_any_entry());
        assert!(!t.emits_on_dispatch());
        assert!(t.emits_on_success());
        assert!(t.emits_on_failure());
    }

    #[test]
    fn timings_round_trip_through_serde() {
        for t in [
            AuditTiming::None,
            AuditTiming::OnDispatch,
            AuditTiming::OnSuccess,
            AuditTiming::OnFailure,
            AuditTiming::Always,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let back: AuditTiming = serde_json::from_str(&json).unwrap();
            assert_eq!(back, t);
        }
    }

    #[test]
    fn always_serialises_to_snake_case() {
        let json = serde_json::to_string(&AuditTiming::Always).unwrap();
        assert_eq!(json, "\"always\"");
    }

    #[test]
    fn on_dispatch_serialises_to_snake_case() {
        let json = serde_json::to_string(&AuditTiming::OnDispatch).unwrap();
        assert_eq!(json, "\"on_dispatch\"");
    }
}
