// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-projection-ws
//!
//! WebSocket projection of the wire-protocol schema.
//!
//! ## Architecture
//!
//! The crate consumes the canonical schema (a slice of
//! `WireOp` values from
//! `evo::projection_schema::canonical_schema()`) and produces
//! three artefacts the runtime mount consumes to stitch the
//! projection onto a concrete WebSocket listener:
//!
//! 1. The **frame-class classifier** ([`classify_op`]): given
//!    a wire op, returns whether it initiates a one-shot
//!    request/response cycle ([`FrameClass::Request`]) or
//!    opens a streaming subscription channel
//!    ([`FrameClass::Subscribe`]). The classifier rule is
//!    prefix-based on the wire op id (`subscribe_*` → Subscribe;
//!    everything else → Request).
//!
//! 2. The **typed frame envelope** ([`envelope`] module): the
//!    JSON-shaped enums for incoming + outgoing frames. The
//!    runtime mount uses serde to (de)serialise; the
//!    per-language SDK generators consume the same shape to
//!    emit typed client bindings.
//!
//! 3. The **dispatch table** ([`WsDispatchTable`]): the
//!    pre-computed lookup map from wire op id to
//!    [`WsDispatchEntry`] (frame class, capability scope,
//!    audit timing, summary). The runtime mount looks up an
//!    incoming op id in O(log n) and dispatches accordingly.
//!
//! ## Single-connection multiplexing
//!
//! Every WebSocket connection multiplexes three frame
//! classes on the same stream:
//!
//! - **Request/Response**: client sends an
//!   [`envelope::IncomingFrame::Request`] with a numeric
//!   `request_id`; server replies with an
//!   [`envelope::OutgoingFrame::Response`] carrying
//!   `response_to = request_id`. One-shot.
//! - **Subscription channel**: client sends an
//!   [`envelope::IncomingFrame::Subscribe`] with a
//!   string `subscription_id`; server emits a stream of
//!   [`envelope::OutgoingFrame::SubscriptionEvent`]
//!   frames carrying that `subscription_id` until the client
//!   sends [`envelope::IncomingFrame::Unsubscribe`] or the
//!   connection closes. The server ack/end the channel via
//!   the corresponding outgoing variants.
//! - **Happenings fan-out**: server emits
//!   [`envelope::OutgoingFrame::Happening`] frames carrying
//!   a monotonic `seq` so reconnecting clients can replay
//!   from a known cursor via the
//!   `subscribe_happenings.since` parameter.
//!
//! ## Listener-agnostic
//!
//! The crate is generation-first: it does not bind a TCP
//! listener, hold connection state, or stream events. The
//! runtime mount that stitches the projection onto a
//! concrete WS listener (axum's `WebSocketUpgrade` + tokio
//! task spawning) owns those concerns.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod dispatch;
pub mod envelope;
pub mod frame_class;
pub mod projection;

pub use dispatch::{WsDispatchEntry, WsDispatchTable};
pub use envelope::{IncomingFrame, OutgoingFrame, ResponseOutcome};
pub use frame_class::{classify_op, FrameClass};
pub use projection::WsProjection;

/// The URL path the WebSocket projection mounts under.
pub const WS_MOUNT_PATH: &str = "/api/v1/ws";
