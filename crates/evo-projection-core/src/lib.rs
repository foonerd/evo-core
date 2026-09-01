// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-projection-core
//!
//! Foundational types for the schema-first multi-projection
//! runtime.
//!
//! ## Architecture
//!
//! The wire protocol that the steward exposes IS the canonical
//! schema. External surfaces (REST, WebSocket, gRPC, GraphQL,
//! HTTP/3 + WebTransport) are generated projections of that
//! schema — never hand-maintained parallel implementations.
//!
//! Industry-standard architecture treats the internal protocol as
//! one contract and hand-builds parallel external surfaces on top.
//! That model produces drift between the internal source-of-truth
//! and the external surfaces every time a new operation lands;
//! adding a new wire op then requires hand-maintained REST
//! endpoints, hand-maintained WebSocket event wrappers, and
//! hand-maintained adapter translation logic in three places.
//! The schema-first architecture this crate anchors removes the
//! drift class entirely: adding a new wire op extends every
//! projection in lockstep because every projection is generated
//! from the same annotated source.
//!
//! ## Contract surface
//!
//! This crate defines the contract every projection and SDK
//! generator consumes:
//!
//! - [`WireOp`] + [`WireOpId`] — the identity and metadata of
//!   one wire operation. A `WireOp` carries its capability scope,
//!   audit timing, and projection-friendly hints alongside its
//!   stable identifier.
//! - [`CapabilityRequirement`] — the access-scope declaration
//!   each op carries. Every projection applies the same scope
//!   gating; a capability-restricted op is restricted uniformly
//!   on REST, WebSocket, gRPC, GraphQL, and HTTP/3.
//! - [`AuditTiming`] — when each op emits to the audit ledger
//!   (no emission / on dispatch / on success / on failure / on
//!   both). The same declaration drives every projection's
//!   audit-emission path.
//! - [`ProjectionContract`] — the trait every concrete
//!   projection (`evo-projection-rest`, `-ws`, `-grpc`,
//!   `-graphql`, `-http3`) implements. Carries the projection's
//!   identity and the set of wire ops it supports.
//! - [`GENERATED_HEADER_RUST`] / [`GENERATED_HEADER_TYPESCRIPT`]
//!   / [`GENERATED_HEADER_PROTO`] / [`GENERATED_HEADER_GRAPHQL`]
//!   — the header invariants for generated projection / SDK
//!   code. A CI pre-commit gate refuses hand-edits to files that
//!   carry these headers (`carries_generated_header`).
//!
//! ## Consumers
//!
//! - **Concrete projections**: `evo-projection-rest` (REST under
//!   `/api/v1/*`), `-ws` (typed WebSocket subscriptions + verbs +
//!   happenings), `-grpc` (proto + service skeletons), `-graphql`
//!   (schema + resolvers), `-http3` (HTTP/3 + WebTransport over
//!   QUIC).
//! - **Client SDK generators**: `evo-sdk-typescript`,
//!   `evo-sdk-swift`, `evo-sdk-kotlin`, `evo-sdk-python`,
//!   `evo-sdk-rust`. Each generator consumes the same annotated
//!   wire schema to emit a typed client library in its target
//!   language.
//!
//! ## Invariants
//!
//! - The wire protocol's typed types in the framework are the
//!   single source of truth. No projection-* crate hand-codes
//!   request / response shapes parallel to the wire types.
//! - Every projection respects the same capability scoping. A
//!   `CapabilityRequirement::StepUp` op refuses on every surface
//!   without active step-up auth, including any future surface
//!   that does not yet exist when the op is authored.
//! - Generated code carries one of the `GENERATED_HEADER_*`
//!   strings as the first non-empty content line. Hand-edits to
//!   files carrying these headers are refused by CI.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audit;
pub mod capability;
pub mod generated_header;
pub mod projection;
pub mod wire_op;

pub use audit::AuditTiming;
pub use capability::CapabilityRequirement;
pub use generated_header::{
    carries_generated_header, GENERATED_HEADER_GRAPHQL, GENERATED_HEADER_PROTO,
    GENERATED_HEADER_RUST, GENERATED_HEADER_TYPESCRIPT,
};
pub use projection::{ProjectionContract, ProjectionId};
pub use wire_op::{WireOp, WireOpId, WireOpIdError};
