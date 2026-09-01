// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`WsProjection`] — the [`ProjectionContract`] impl for the
//! WebSocket projection.

use crate::dispatch::WsDispatchTable;
use evo_projection_core::{ProjectionContract, ProjectionId, WireOp, WireOpId};

/// The WebSocket projection.
///
/// Built once from the canonical schema; holds the dispatch
/// table the runtime mount consults on every incoming frame.
#[derive(Debug, Clone)]
pub struct WsProjection {
    dispatch: WsDispatchTable,
}

impl WsProjection {
    /// Construct a `WsProjection` from a wire schema slice.
    pub fn from_schema(schema: &[WireOp]) -> Self {
        Self {
            dispatch: WsDispatchTable::from_schema(schema),
        }
    }

    /// Borrow the underlying dispatch table.
    pub fn dispatch_table(&self) -> &WsDispatchTable {
        &self.dispatch
    }

    /// The total number of ops bound on this projection.
    pub fn op_count(&self) -> usize {
        self.dispatch.op_count()
    }
}

impl ProjectionContract for WsProjection {
    fn projection_id(&self) -> ProjectionId {
        ProjectionId::WebSocket
    }

    fn supported_ops(&self) -> Vec<WireOpId> {
        self.dispatch
            .entries()
            .values()
            .map(|e| e.op_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn schema_with(n: usize) -> Vec<WireOp> {
        (0..n)
            .map(|i| {
                WireOp::new(
                    WireOpId::new(format!("op_{}", i)).unwrap(),
                    CapabilityRequirement::None,
                    AuditTiming::None,
                    "fixture",
                )
            })
            .collect()
    }

    #[test]
    fn projection_reports_websocket_identity() {
        let p = WsProjection::from_schema(&schema_with(3));
        assert_eq!(p.projection_id(), ProjectionId::WebSocket);
    }

    #[test]
    fn projection_op_count_matches_input() {
        let p = WsProjection::from_schema(&schema_with(124));
        assert_eq!(p.op_count(), 124);
    }

    #[test]
    fn projection_supported_ops_match_input_set() {
        let schema = schema_with(5);
        let p = WsProjection::from_schema(&schema);
        let supported: std::collections::HashSet<_> =
            p.supported_ops().into_iter().collect();
        let expected: std::collections::HashSet<_> =
            schema.iter().map(|op| op.id.clone()).collect();
        assert_eq!(supported, expected);
    }

    #[test]
    fn projection_supports_empty_schema() {
        let p = WsProjection::from_schema(&[]);
        assert_eq!(p.op_count(), 0);
        assert!(p.supported_ops().is_empty());
    }
}
