// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-projection-rest
//!
//! REST projection of the wire-protocol schema.
//!
//! ## Architecture
//!
//! The crate consumes the canonical schema (a slice of
//! `WireOp` values produced by the framework's
//! `projection_schema::canonical_schema()`) and generates the
//! in-memory REST endpoint table: one [`RestEndpoint`] per
//! wire op, carrying the derived HTTP method, the URL path,
//! and the original [`WireOp`] metadata for capability /
//! audit dispatch.
//!
//! The crate is generation-first and listener-agnostic: it
//! does not bind a TCP listener, parse request bodies, or
//! serialise response payloads. Those concerns belong to the
//! runtime mount that stitches the endpoint table onto a
//! concrete HTTP server (axum + tower-http). Keeping the
//! generator pure simplifies testing (no I/O, no async) and
//! lets the runtime mount evolve independently of the schema-
//! to-endpoint rules.
//!
//! ## Derivation rules
//!
//! ### Method
//!
//! HTTP method derivation is rule-based on the wire op's
//! capability scope and identifier prefix:
//!
//! - Anonymous + `Read`-scope ops → `GET`
//! - `delete_*` / `cancel_*` / `revoke_*` / `remove_*` → `DELETE`
//! - `put_*` / `set_*` → `PUT`
//! - Everything else (Write / StepUp default) → `POST`
//!
//! See [`derive_method`](method::derive_method) for the
//! authoritative implementation; the tests in `method` cover
//! every wire op in the framework's canonical schema.
//!
//! ### Path
//!
//! URL paths are derived directly from the wire op id:
//!
//! - Default: `/api/v1/<op_id>` (1:1 with the wire schema)
//!
//! Direct mapping is the simplest and most operator-debuggable
//! shape: the REST endpoint name equals the wire op name on
//! every observability surface (logs, traces, metrics, audit
//! ledger). Resource-oriented REST paths (`PUT
//! /api/v1/updates/channel` instead of `POST
//! /api/v1/set_update_channel`) are achievable via a future
//! parallel `evo-projection-rest-resource` generator that
//! reuses the schema; this crate ships the direct mapping as
//! the schema-first reference.
//!
//! ## Streaming ops
//!
//! `subscribe_*` ops live at the same path as every other op;
//! the runtime mount negotiates streaming via the `Accept`
//! header (`text/event-stream` for Server-Sent Events,
//! `application/json` for one-shot polling collapse). The
//! [`RestEndpoint`] does not encode the streaming distinction;
//! the runtime classifies it from the wire op id.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod endpoint;
pub mod generator;
pub mod method;
pub mod path;
pub mod projection;

pub use endpoint::RestEndpoint;
pub use generator::generate_endpoints;
pub use method::{derive_method, HttpMethod};
pub use path::rest_path_for;
pub use projection::RestProjection;
