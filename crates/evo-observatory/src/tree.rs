// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Span-tree reconstruction.
//!
//! Observations land in the ring flat; consumers asking for
//! "the trace tree of request X" call into this module with
//! the ring's snapshot + a trace root. The reconstruction
//! groups observations by `trace_root`, indexes them by
//! `span_id`, and stitches by `parent_span_id` to yield a
//! [`SpanTreeNode`] tree rooted at the requested trace.
//!
//! The tree is read-only and built per query — no mutable
//! shared state. The cost is O(n) over the snapshot length,
//! which is the right cost model for the ring sizes we run.

use crate::observation::Observation;
use crate::span::SpanId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One node in a reconstructed span tree.
///
/// Carries every observation that belongs to one span,
/// ordered by timestamp, plus a list of child span trees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanTreeNode {
    /// This span's identifier.
    pub span_id: SpanId,

    /// Every observation that carried this `span_id`,
    /// ordered by timestamp. Typically two — a "started"
    /// kind followed by a closing kind — but the surface
    /// supports any number so a single span can emit
    /// multiple markers.
    pub observations: Vec<Observation>,

    /// Children, recursively. Ordered by the first
    /// observation timestamp on each child so the tree
    /// renders chronologically left-to-right.
    pub children: Vec<SpanTreeNode>,
}

/// Reconstruct the tree rooted at `trace_root` from a flat
/// list of observations. Returns `None` if no observation
/// in the list belongs to that trace.
pub fn build_span_tree(
    observations: &[Observation],
    trace_root: SpanId,
) -> Option<SpanTreeNode> {
    // Filter to one trace.
    let in_trace: Vec<&Observation> = observations
        .iter()
        .filter(|o| o.span.trace_root == trace_root)
        .collect();
    if in_trace.is_empty() {
        return None;
    }

    // Index span observations by span_id.
    let mut by_span: HashMap<SpanId, Vec<&Observation>> = HashMap::new();
    for obs in &in_trace {
        by_span.entry(obs.span.span_id).or_default().push(obs);
    }

    // Within each span, sort by timestamp so observations
    // render in emission order.
    for list in by_span.values_mut() {
        list.sort_by_key(|o| o.ts_ns);
    }

    // Index children by parent.
    let mut children_of: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
    for span_id in by_span.keys() {
        if let Some(parent) = parent_of(&by_span, *span_id) {
            children_of.entry(parent).or_default().push(*span_id);
        }
    }

    Some(materialise(trace_root, &by_span, &children_of))
}

fn parent_of(
    by_span: &HashMap<SpanId, Vec<&Observation>>,
    span_id: SpanId,
) -> Option<SpanId> {
    by_span
        .get(&span_id)
        .and_then(|obs| obs.first())
        .and_then(|o| o.span.parent_span_id)
}

fn materialise(
    span_id: SpanId,
    by_span: &HashMap<SpanId, Vec<&Observation>>,
    children_of: &HashMap<SpanId, Vec<SpanId>>,
) -> SpanTreeNode {
    let observations: Vec<Observation> = by_span
        .get(&span_id)
        .map(|v| v.iter().map(|o| (*o).clone()).collect())
        .unwrap_or_default();

    let mut children: Vec<SpanTreeNode> = children_of
        .get(&span_id)
        .map(|kids| {
            kids.iter()
                .map(|k| materialise(*k, by_span, children_of))
                .collect()
        })
        .unwrap_or_default();

    // Order children by their first observation's
    // timestamp so the tree reads left-to-right in
    // chronological order.
    children.sort_by_key(|child| {
        child
            .observations
            .first()
            .map(|o| o.ts_ns)
            .unwrap_or(u128::MAX)
    });

    SpanTreeNode {
        span_id,
        observations,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::ObservationKind;
    use crate::observation::Outcome;
    use crate::span::SpanContext;

    fn obs_at(ts_ns: u128, span: SpanContext) -> Observation {
        let mut o = Observation::now(
            span,
            ObservationKind::Marker,
            Outcome::Informational,
        );
        o.ts_ns = ts_ns;
        o
    }

    #[test]
    fn tree_reconstructs_simple_chain() {
        let root = SpanContext::new_root();
        let child = root.child();
        let grand = child.child();
        let flat = vec![obs_at(1, root), obs_at(2, child), obs_at(3, grand)];
        let tree = build_span_tree(&flat, root.trace_root).unwrap();
        assert_eq!(tree.span_id, root.span_id);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].span_id, child.span_id);
        assert_eq!(tree.children[0].children.len(), 1);
        assert_eq!(tree.children[0].children[0].span_id, grand.span_id);
    }

    #[test]
    fn tree_reconstructs_fan_out() {
        let root = SpanContext::new_root();
        let a = root.child();
        let b = root.child();
        let c = root.child();
        let flat =
            vec![obs_at(1, root), obs_at(3, a), obs_at(2, b), obs_at(4, c)];
        let tree = build_span_tree(&flat, root.trace_root).unwrap();
        assert_eq!(tree.children.len(), 3);
        // Children ordered chronologically: b (2), a (3), c (4).
        assert_eq!(tree.children[0].span_id, b.span_id);
        assert_eq!(tree.children[1].span_id, a.span_id);
        assert_eq!(tree.children[2].span_id, c.span_id);
    }

    #[test]
    fn tree_orders_observations_within_span_chronologically() {
        let root = SpanContext::new_root();
        let flat = vec![obs_at(3, root), obs_at(1, root), obs_at(2, root)];
        let tree = build_span_tree(&flat, root.trace_root).unwrap();
        let ts: Vec<u128> = tree.observations.iter().map(|o| o.ts_ns).collect();
        assert_eq!(ts, vec![1, 2, 3]);
    }

    #[test]
    fn tree_excludes_observations_from_other_traces() {
        let root_a = SpanContext::new_root();
        let root_b = SpanContext::new_root();
        let flat = vec![obs_at(1, root_a), obs_at(2, root_b)];
        let tree = build_span_tree(&flat, root_a.trace_root).unwrap();
        assert_eq!(tree.observations.len(), 1);
        assert!(tree.children.is_empty());
    }

    #[test]
    fn tree_returns_none_for_unknown_trace_root() {
        let root = SpanContext::new_root();
        let flat = vec![obs_at(1, root)];
        let absent = SpanId::from_u128(0xabcd);
        assert!(build_span_tree(&flat, absent).is_none());
    }

    #[test]
    fn tree_handles_orphan_child_with_missing_parent_observation() {
        // The parent's observation never made it into the
        // window (rotated out by wrap), but a child observation
        // exists. The reconstructor still surfaces the trace,
        // rooted at the child's parent_id slot, with the child
        // as a known descendant.
        let root = SpanContext::new_root();
        let child = root.child();
        // Only the child observation present.
        let flat = vec![obs_at(1, child)];
        let tree = build_span_tree(&flat, root.trace_root).unwrap();
        // The root node carries no observations (parent
        // missing) but the child still attaches.
        assert!(tree.observations.is_empty());
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].span_id, child.span_id);
    }
}
