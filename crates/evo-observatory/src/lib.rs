// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-observatory — the Owl
//!
//! Substrate-level structural observability. Every TLS
//! handshake, every cert event, every bearer-token issue /
//! verify / revoke, every dispatch decision, every
//! capability gate, every response — all captured as a typed
//! [`Observation`] carrying a causal span chain.
//!
//! ## What sets this apart
//!
//! Industry-standard observability is opt-in instrumentation
//! sitting alongside the substrate (OpenTelemetry, log
//! collectors, sidecar Grafana). This crate is the inverse:
//! observability *is* a substrate, mounted at the same trust
//! boundary as the HTTPS listener, exposed through the same
//! capability-gated wire surface, available without an
//! external collector. Operators read the trace tree of any
//! recent request live, from the same endpoint the request
//! itself was served from.
//!
//! ## Design properties
//!
//! - **Silent when off.** The `enabled` feature gates every
//!   public entry point; with the feature disabled, the
//!   public surface compiles to no-ops and the crate holds
//!   no allocations. The substrate is paid for only when it
//!   is mounted.
//! - **Single-AMO push.** When enabled, producers append by
//!   reserving a slot through one atomic fetch-add on the
//!   tail cursor. No locks. No per-call allocation in the
//!   steady state — the ring's slots own pre-sized payload
//!   buffers and reuse them.
//! - **Bounded memory.** The ring has a fixed slot count
//!   chosen at construction. Overflow overwrites the oldest
//!   observation; the read API returns the slots that are
//!   currently valid plus a wrap counter so consumers can
//!   detect gaps.
//! - **Causal span tree.** Every observation carries a
//!   `span_id` and an optional `parent_span_id`; the
//!   read API reconstructs the full ancestor/descendant tree
//!   on demand. Causality is not opt-in instrumentation; it
//!   is a property of the substrate emission seams.
//! - **Decline-cause attribution.** Every refused request,
//!   every failed dispatch, every TLS handshake error
//!   carries a structured cause — not a string, a typed
//!   [`DeclineCause`] variant — so "why did this 403?" is
//!   answerable from a single observation, no log
//!   correlation required.
//! - **Stable wire shape.** Observations serialise as JSON
//!   with stable field names; consumers (the operator UI,
//!   external tooling) treat the schema as a forward-
//!   compatible contract.
//!
//! ## Substrate emission seams
//!
//! Each downstream substrate emits at well-defined seams:
//!
//! - `evo-tls-certs`: CA generation, leaf issuance, manual
//!   bundle load, rotation.
//! - `evo-auth-bearer`: token issue, verify success / fail,
//!   revoke, capability mismatch.
//! - `evo-runtime-http`: TLS handshake start / complete /
//!   fail, request received, capability decision, dispatch
//!   start / end, response written.
//! - the steward bridge: dispatch entry / exit, internal
//!   error, plugin invocation.
//!
//! All seams produce the same `Observation` type. The
//! operator's view of the system is substrate-blind:
//! whether a span originated in TLS, the auth layer, the
//! dispatcher, or a plugin, it reads back through the same
//! ring and the same tree-reconstruction logic.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod attr;
pub mod cause;
pub mod kind;
pub mod observation;
pub mod observatory;
pub mod span;
pub mod tree;

pub use attr::{AttrKey, AttrValue, Attributes};
pub use cause::{BearerReason, DeclineCause, TlsHandshakeReason};
pub use kind::ObservationKind;
pub use observation::{Observation, Outcome};
pub use observatory::{Observatory, ObservatoryConfig, ObservatoryStats};
pub use span::{SpanContext, SpanId};
pub use tree::SpanTreeNode;
