// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-projection-http3
//!
//! HTTP/3 + WebTransport projection of the wire-protocol
//! schema.
//!
//! ## Architecture
//!
//! HTTP/3 carries HTTP semantics over QUIC. Request-class wire
//! ops project onto HTTP/3 with the same method + path the
//! REST projection emits (the runtime mount serves them at the
//! same URL space on the HTTP/3 port). Subscribe-class wire ops
//! project onto WebTransport bidirectional streams multiplexed
//! within one WebTransport session at a single URL; the
//! per-stream first frame declares which subscription op the
//! stream is opening.
//!
//! Two transport behaviours under the same TLS:
//!
//! - **HTTP/3 request-response** (Request-class ops):
//!   identical method + path to the REST projection. The HTTP
//!   semantic layer is unchanged; only the transport (QUIC vs
//!   TCP) differs.
//! - **WebTransport bidirectional streams** (Subscribe-class
//!   ops): client and server negotiate one WebTransport
//!   session via an HTTP/3 CONNECT request to
//!   [`WT_SESSION_PATH`]; subscriptions then open as
//!   bidirectional streams within the session. The first
//!   frame on each stream carries the subscription parameters
//!   (`{op, subscription_id, payload}`); subsequent frames
//!   from server to client carry the event stream until the
//!   stream is closed.
//!
//! ## Listener-agnostic
//!
//! The crate generates the typed endpoint table. It does not
//! bind UDP, negotiate QUIC, or implement the WebTransport
//! handshake — those concerns belong to the runtime mount
//! that stitches the projection onto a concrete HTTP/3 server.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod endpoint;
pub mod generator;
pub mod projection;

pub use endpoint::{Http3Endpoint, Http3SurfaceKind};
pub use generator::generate_endpoints;
pub use projection::Http3Projection;

/// The default HTTP/3 + WebTransport listening port.
///
/// Distinct from 443 (HTTP/2 + WebSocket) so an operator can
/// run both transports concurrently. The runtime mount
/// advertises this port via the HTTP/2 listener's `Alt-Svc`
/// header so HTTP/3-capable clients discover and upgrade.
pub const HTTP3_PORT: u16 = 4433;

/// The WebTransport session URL path.
///
/// HTTP/3 clients negotiate a WebTransport session via an
/// extended-CONNECT request to this path; the resulting
/// session multiplexes every active subscription stream for
/// the connection.
pub const WT_SESSION_PATH: &str = "/api/v1/wt";
