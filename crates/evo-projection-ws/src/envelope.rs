// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Typed JSON envelope for WS frames.
//!
//! The envelope is the projection's wire-level contract. The
//! runtime mount serialises every outgoing frame and parses
//! every incoming frame through these types; per-language
//! client SDK generators emit the equivalent shapes in their
//! target language.
//!
//! ## Tag discipline
//!
//! Every frame variant is internally tagged with a
//! `frame_type` field in snake-case. Unknown frame types are
//! rejected at parse time via `deny_unknown_fields` at the
//! envelope level so a typo on the client surfaces as a
//! structured parse error rather than as a confusing
//! downstream dispatch failure.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// One frame from a client to the server.
///
/// `payload` carries the wire op's typed parameters as a JSON
/// object; the runtime mount validates the payload against the
/// op's parameter schema before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame_type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum IncomingFrame {
    /// One-shot request. The server emits exactly one
    /// matching [`OutgoingFrame::Response`] carrying
    /// `response_to = request_id`.
    Request {
        /// Caller-chosen correlation id. Must be unique
        /// across in-flight requests on this connection;
        /// the server uses it to match responses.
        request_id: u64,
        /// Wire op id from the canonical schema.
        op: String,
        /// Op-specific parameters as a JSON object. Defaults
        /// to an empty object when omitted.
        #[serde(default = "default_payload")]
        payload: JsonValue,
    },

    /// Open a subscription channel. The server replies with
    /// one [`OutgoingFrame::SubscriptionAck`] then emits a
    /// stream of [`OutgoingFrame::SubscriptionEvent`] frames
    /// keyed by `subscription_id` until the client unsubscribes
    /// or the connection closes.
    Subscribe {
        /// Caller-chosen channel id. Must be unique across
        /// active subscriptions on this connection; the
        /// server uses it to key emitted events back to the
        /// channel.
        subscription_id: String,
        /// Wire op id from the canonical schema. Must be a
        /// `subscribe_*` op; the runtime mount rejects
        /// Subscribe frames carrying a request-class op id.
        op: String,
        /// Op-specific parameters as a JSON object.
        #[serde(default = "default_payload")]
        payload: JsonValue,
    },

    /// Close a subscription channel. The server emits one
    /// matching [`OutgoingFrame::SubscriptionEnded`] and stops
    /// dispatching events for the channel.
    Unsubscribe {
        /// The id of the channel to close. Must match an
        /// active subscription on this connection.
        subscription_id: String,
    },
}

/// One frame from the server to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame_type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum OutgoingFrame {
    /// Reply to a one-shot [`IncomingFrame::Request`].
    Response {
        /// The `request_id` of the originating frame.
        response_to: u64,
        /// Outcome — `ok` with a value, or `err` with a
        /// structured code + message.
        outcome: ResponseOutcome,
    },

    /// Acknowledge that a subscription channel has opened.
    /// Emitted exactly once per Subscribe.
    SubscriptionAck {
        /// The `subscription_id` of the channel.
        subscription_id: String,
        /// Initial payload delivered inline on the ack when
        /// the subscribe request opted into snapshot-in-ack
        /// via a payload field the dispatcher recognises
        /// (`subscribe_subject.snapshot_in_ack: true` is the
        /// documented consumer). Absent for legacy consumers
        /// that did not opt in — the wire shape stays
        /// byte-identical for pre-snapshot-in-ack clients.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_event: Option<JsonValue>,
    },

    /// One event on an open subscription channel.
    SubscriptionEvent {
        /// The `subscription_id` keying this event back to
        /// the channel.
        subscription_id: String,
        /// The event payload. Shape is op-specific; the
        /// per-language SDK generators emit typed event
        /// classes per subscription op id.
        event: JsonValue,
    },

    /// Notify the client that a subscription channel has
    /// closed.
    SubscriptionEnded {
        /// The `subscription_id` of the closed channel.
        subscription_id: String,
        /// Operator-readable reason — `"unsubscribed"`,
        /// `"connection_closed"`, `"server_terminated"`, or
        /// a structured error code from the dispatcher.
        reason: String,
    },

    /// A happening from the framework's durable bus.
    /// Independent of any client subscription; the runtime
    /// mount fans this out to every connection that has
    /// opened a `subscribe_happenings` channel.
    Happening {
        /// Monotonic sequence number from the durable bus.
        /// Clients may persist the last seen `seq` and
        /// resume via the `since` parameter on the next
        /// connection's `subscribe_happenings`.
        seq: u64,
        /// The happening payload.
        happening: JsonValue,
    },
}

/// Outcome of a one-shot request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ResponseOutcome {
    /// Success.
    Ok {
        /// The op's typed response payload.
        value: JsonValue,
    },
    /// Failure.
    Err {
        /// Structured error code from the framework's error
        /// taxonomy (e.g. `"permission_denied"`,
        /// `"step_up_required"`, `"unknown_op"`).
        code: String,
        /// Operator-readable error message.
        message: String,
    },
}

fn default_payload() -> JsonValue {
    JsonValue::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_request_serialises_with_payload() {
        let frame = IncomingFrame::Request {
            request_id: 42,
            op: "list_plugins".into(),
            payload: serde_json::json!({"filter": "audio"}),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: IncomingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn incoming_request_defaults_payload_to_empty_object() {
        let frame: IncomingFrame = serde_json::from_str(
            r#"{"frame_type":"request","request_id":1,"op":"describe_capabilities"}"#,
        )
        .unwrap();
        match frame {
            IncomingFrame::Request { payload, .. } => {
                assert_eq!(payload, serde_json::json!({}));
            }
            _ => panic!("expected Request variant"),
        }
    }

    #[test]
    fn incoming_subscribe_serialises_with_subscription_id() {
        let frame = IncomingFrame::Subscribe {
            subscription_id: "happenings-1".into(),
            op: "subscribe_happenings".into(),
            payload: serde_json::json!({"since": 100}),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: IncomingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn incoming_unsubscribe_serialises_minimally() {
        let frame = IncomingFrame::Unsubscribe {
            subscription_id: "happenings-1".into(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: IncomingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn outgoing_response_ok_carries_typed_value() {
        let frame = OutgoingFrame::Response {
            response_to: 42,
            outcome: ResponseOutcome::Ok {
                value: serde_json::json!({"plugins": []}),
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: OutgoingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn outgoing_response_err_carries_code_and_message() {
        let frame = OutgoingFrame::Response {
            response_to: 42,
            outcome: ResponseOutcome::Err {
                code: "permission_denied".into(),
                message: "capability scope missing: plugins_admin".into(),
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: OutgoingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn outgoing_subscription_ack_round_trips() {
        let frame = OutgoingFrame::SubscriptionAck {
            subscription_id: "happenings-1".into(),
            initial_event: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: OutgoingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn outgoing_subscription_ack_omits_initial_event_when_absent() {
        // Backward-compat: legacy consumer sees byte-identical
        // ack — the `initial_event` field is skipped when
        // absent, not serialised as `"initial_event":null`.
        let frame = OutgoingFrame::SubscriptionAck {
            subscription_id: "happenings-1".into(),
            initial_event: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            !json.contains("initial_event"),
            "legacy ack must omit the initial_event field entirely, \
             got: {json}"
        );
    }

    #[test]
    fn outgoing_subscription_ack_carries_initial_event_when_present() {
        let projection = serde_json::json!({
            "canonical_id": "abc",
            "state": {"foo": "bar"}
        });
        let frame = OutgoingFrame::SubscriptionAck {
            subscription_id: "subject-1".into(),
            initial_event: Some(projection.clone()),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["frame_type"], "subscription_ack");
        assert_eq!(v["subscription_id"], "subject-1");
        assert_eq!(v["initial_event"], projection);
    }

    #[test]
    fn outgoing_subscription_event_carries_event_payload() {
        let frame = OutgoingFrame::SubscriptionEvent {
            subscription_id: "happenings-1".into(),
            event: serde_json::json!({"kind": "custody_taken", "details": {}}),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: OutgoingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn outgoing_subscription_ended_carries_reason() {
        let frame = OutgoingFrame::SubscriptionEnded {
            subscription_id: "happenings-1".into(),
            reason: "unsubscribed".into(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: OutgoingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn outgoing_happening_carries_seq_and_payload() {
        let frame = OutgoingFrame::Happening {
            seq: 1234,
            happening: serde_json::json!({"kind": "ui_shelf_changed"}),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: OutgoingFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn unknown_incoming_frame_type_refuses() {
        let result: Result<IncomingFrame, _> = serde_json::from_str(
            r#"{"frame_type":"floop","request_id":1,"op":"x"}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn unknown_outgoing_frame_type_refuses() {
        let result: Result<OutgoingFrame, _> =
            serde_json::from_str(r#"{"frame_type":"floop","response_to":1}"#);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_field_on_incoming_request_refuses() {
        let result: Result<IncomingFrame, _> = serde_json::from_str(
            r#"{"frame_type":"request","request_id":1,"op":"x","mystery_field":42}"#,
        );
        assert!(result.is_err());
    }
}
