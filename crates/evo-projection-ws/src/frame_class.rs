// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Frame-class classification.
//!
//! Every wire op falls into one of two WS frame classes:
//! one-shot request/response, or streaming subscription. The
//! classifier rule is deliberately simple — a prefix match on
//! the wire op id — so the mapping is predictable from the
//! schema without consulting the classifier code.

use evo_projection_core::WireOp;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The WS frame class for one wire op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameClass {
    /// One-shot request initiating a Request/Response cycle.
    /// The server emits a single
    /// [`crate::envelope::OutgoingFrame::Response`] in reply
    /// and the cycle terminates.
    Request,

    /// Subscription start. The server emits a stream of
    /// [`crate::envelope::OutgoingFrame::SubscriptionEvent`]
    /// frames on the channel keyed by `subscription_id` until
    /// the client unsubscribes or the connection closes.
    Subscribe,
}

impl FrameClass {
    /// The canonical snake-case string identifier for this
    /// class.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Subscribe => "subscribe",
        }
    }
}

impl fmt::Display for FrameClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify a wire op for WS dispatch.
///
/// Rule: an op id starting with `subscribe_` maps to
/// [`FrameClass::Subscribe`]; everything else maps to
/// [`FrameClass::Request`].
pub fn classify_op(op: &WireOp) -> FrameClass {
    if op.id.as_str().starts_with("subscribe_") {
        FrameClass::Subscribe
    } else {
        FrameClass::Request
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn make_op(id: &str) -> WireOp {
        WireOp::new(
            WireOpId::new(id).unwrap(),
            CapabilityRequirement::None,
            AuditTiming::None,
            "test",
        )
    }

    #[test]
    fn frame_class_as_str_returns_snake_case() {
        assert_eq!(FrameClass::Request.as_str(), "request");
        assert_eq!(FrameClass::Subscribe.as_str(), "subscribe");
    }

    #[test]
    fn frame_class_display_matches_as_str() {
        for c in [FrameClass::Request, FrameClass::Subscribe] {
            assert_eq!(format!("{}", c), c.as_str());
        }
    }

    #[test]
    fn frame_class_round_trips_through_serde() {
        for c in [FrameClass::Request, FrameClass::Subscribe] {
            let json = serde_json::to_string(&c).unwrap();
            let back: FrameClass = serde_json::from_str(&json).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn subscribe_happenings_classifies_as_subscribe() {
        assert_eq!(
            classify_op(&make_op("subscribe_happenings")),
            FrameClass::Subscribe
        );
    }

    #[test]
    fn subscribe_subject_classifies_as_subscribe() {
        assert_eq!(
            classify_op(&make_op("subscribe_subject")),
            FrameClass::Subscribe
        );
    }

    #[test]
    fn describe_capabilities_classifies_as_request() {
        assert_eq!(
            classify_op(&make_op("describe_capabilities")),
            FrameClass::Request
        );
    }

    #[test]
    fn list_plugins_classifies_as_request() {
        assert_eq!(classify_op(&make_op("list_plugins")), FrameClass::Request);
    }

    #[test]
    fn set_update_channel_classifies_as_request() {
        assert_eq!(
            classify_op(&make_op("set_update_channel")),
            FrameClass::Request
        );
    }

    #[test]
    fn substring_subscribe_does_not_match_without_prefix() {
        // A hypothetical op id containing `subscribe` somewhere
        // other than the prefix must classify as Request.
        // (No wire op like this exists today; the rule must
        // hold prophylactically.)
        assert_eq!(
            classify_op(&make_op("list_active_subscriptions")),
            FrameClass::Request
        );
    }
}
