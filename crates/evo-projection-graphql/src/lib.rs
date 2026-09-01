// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-projection-graphql
//!
//! GraphQL projection of the wire-protocol schema.
//!
//! ## Architecture
//!
//! The crate emits the canonical GraphQL schema (`.graphqls`
//! file content) from the wire schema: one `Query` root for
//! read / anonymous ops, one `Mutation` root for write /
//! step-up ops, one `Subscription` root for subscribe-class
//! ops. The emitted schema string is consumed by the runtime
//! mount that stitches the projection onto a concrete GraphQL
//! server (typically async-graphql with a runtime-registered
//! `JSON` scalar resolver).
//!
//! ## Root-type partition
//!
//! Each wire op lands on exactly one root type:
//!
//! - Subscribe-class (`subscribe_*` prefix) → `Subscription`
//! - Else write or step-up capability → `Mutation`
//! - Else (anonymous or read capability) → `Query`
//!
//! See [`classify_root`](root::classify_root) for the
//! authoritative implementation.
//!
//! ## Field naming
//!
//! GraphQL convention is camelCase for fields. The emitter
//! converts the snake_case wire op id via
//! [`field_name::camel_case_field_name`]:
//! `describe_capabilities` → `describeCapabilities`.
//!
//! ## Payload typing
//!
//! Op payloads are op-specific JSON, expressed in GraphQL as
//! the community-standard `JSON` scalar (registered by the
//! runtime mount's resolver layer). Every field takes one
//! `payload: JSON` argument and returns a `WireOpResult`
//! union (either `WireOpValue` carrying the result JSON, or
//! `WireOpError` carrying a structured code + message).
//! Subscription fields return a `SubscriptionEvent` shape
//! that carries the per-event JSON plus the durable bus seq
//! for cursor replay.
//!
//! ## Listener-agnostic
//!
//! The crate emits the schema text. It does not run an
//! async-graphql server, register resolvers, or bind a TCP
//! listener — those concerns belong to the runtime mount.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod emit;
pub mod field_name;
pub mod projection;
pub mod root;

pub use emit::{emit_schema, GraphQlEmitConfig};
pub use field_name::camel_case_field_name;
pub use projection::GraphQlProjection;
pub use root::{classify_root, RootType};
