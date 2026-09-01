// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The [`Observation`] record — one event captured by an
//! emission seam.

use crate::attr::Attributes;
use crate::cause::DeclineCause;
use crate::kind::ObservationKind;
use crate::span::SpanContext;
use serde::{Deserialize, Serialize};

/// Outcome of an observed operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The observation reports an open span — the operation
    /// is in progress, no terminal state yet. Used for the
    /// "started" kinds (handshake started, dispatch
    /// started).
    Started,
    /// The operation completed successfully.
    Success,
    /// The operation was refused or failed. The
    /// observation's [`Observation::cause`] field carries
    /// the structured reason.
    Declined,
    /// The observation is informational — no operation
    /// outcome to record (markers, configuration
    /// snapshots).
    Informational,
}

/// One observation captured by a substrate emission seam.
///
/// Wire shape is stable and consumed by the operator UI +
/// external tooling. Fields are explicit so additions are
/// visible in diffs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Monotonic-ish wall-clock nanoseconds at emission.
    /// Wall-clock is sufficient for trace ordering within
    /// a process; consumers reconciling across hosts use
    /// the span chain, not the timestamp.
    pub ts_ns: u128,

    /// Causal span context this observation belongs to.
    /// The span_tree reconstructor groups observations by
    /// `trace_root` and stitches by `parent_span_id`.
    pub span: SpanContext,

    /// The kind of seam that produced this observation.
    pub kind: ObservationKind,

    /// Outcome at emission time.
    pub outcome: Outcome,

    /// Wire op id when this observation belongs to a
    /// canonical wire operation. Empty for substrate-only
    /// observations (TLS handshakes, cert events) that do
    /// not correspond to one specific wire op.
    pub op_id: String,

    /// Token id of the authenticated principal, when
    /// applicable. Empty when the seam runs without a
    /// principal (anonymous routes, internal substrate
    /// emissions).
    pub principal_token_id: String,

    /// Structured attributes specific to this kind.
    pub attrs: Attributes,

    /// Structured cause when `outcome == Declined`. `None`
    /// otherwise. Consumers can read `cause` to answer
    /// "why was this declined?" without grepping any
    /// secondary log surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<DeclineCause>,

    /// Wall-clock micros measured between the matching
    /// "started" observation and this one, when the seam
    /// is a span-closer. Zero for "started" / "marker"
    /// observations.
    pub latency_us: u64,
}

impl Observation {
    /// Build a fresh observation with the current
    /// timestamp.
    pub fn now(
        span: SpanContext,
        kind: ObservationKind,
        outcome: Outcome,
    ) -> Self {
        Self {
            ts_ns: current_ts_ns(),
            span,
            kind,
            outcome,
            op_id: String::new(),
            principal_token_id: String::new(),
            attrs: Attributes::new(),
            cause: None,
            latency_us: 0,
        }
    }

    /// Builder: attach the wire op id.
    pub fn with_op_id(mut self, op_id: impl Into<String>) -> Self {
        self.op_id = op_id.into();
        self
    }

    /// Builder: attach the principal token id.
    pub fn with_principal_token_id(
        mut self,
        token_id: impl Into<String>,
    ) -> Self {
        self.principal_token_id = token_id.into();
        self
    }

    /// Builder: attach attributes.
    pub fn with_attrs(mut self, attrs: Attributes) -> Self {
        self.attrs = attrs;
        self
    }

    /// Builder: attach a decline cause.
    pub fn with_cause(mut self, cause: DeclineCause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// Builder: attach a latency measurement.
    pub fn with_latency_us(mut self, latency_us: u64) -> Self {
        self.latency_us = latency_us;
        self
    }
}

/// Sample the current wall clock in nanoseconds since the
/// UNIX epoch.
fn current_ts_ns() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr::AttrValue;
    use crate::cause::{BearerReason, DeclineCause};

    #[test]
    fn now_sets_a_recent_timestamp() {
        let span = SpanContext::new_root();
        let before = current_ts_ns();
        let obs = Observation::now(
            span,
            ObservationKind::Marker,
            Outcome::Informational,
        );
        let after = current_ts_ns();
        assert!(obs.ts_ns >= before);
        assert!(obs.ts_ns <= after);
    }

    #[test]
    fn builders_compose() {
        let span = SpanContext::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::CapabilityDeclined,
            Outcome::Declined,
        )
        .with_op_id("install_plugin")
        .with_principal_token_id("tok-1")
        .with_attrs(Attributes::new().with("bytes_in", 42u64))
        .with_cause(DeclineCause::Bearer {
            reason: BearerReason::Expired,
            token_id: "tok-1".to_string(),
        })
        .with_latency_us(125);

        assert_eq!(obs.op_id, "install_plugin");
        assert_eq!(obs.principal_token_id, "tok-1");
        assert_eq!(obs.attrs.get("bytes_in"), Some(&AttrValue::UInt(42)));
        assert!(matches!(obs.cause, Some(DeclineCause::Bearer { .. })));
        assert_eq!(obs.latency_us, 125);
    }

    #[test]
    fn observation_round_trips_through_serde() {
        let span = SpanContext::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::CapabilityAdmitted,
            Outcome::Success,
        )
        .with_op_id("list_plugins")
        .with_principal_token_id("tok")
        .with_attrs(Attributes::new().with("granted", "plugins"))
        .with_latency_us(42);
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, obs);
    }

    #[test]
    fn cause_field_is_skipped_when_none() {
        let span = SpanContext::new_root();
        let obs = Observation::now(
            span,
            ObservationKind::DispatchStarted,
            Outcome::Started,
        );
        let json = serde_json::to_string(&obs).unwrap();
        assert!(
            !json.contains("\"cause\""),
            "absent cause must not surface on the wire"
        );
    }
}
