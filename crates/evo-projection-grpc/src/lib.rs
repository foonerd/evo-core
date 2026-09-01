// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-projection-grpc
//!
//! gRPC projection of the wire-protocol schema.
//!
//! ## Architecture
//!
//! The crate emits the canonical `.proto` file content from the
//! wire schema: one `service Evo { ... }` carrying every wire
//! op as an RPC method, plus the generic `WireOpRequest` /
//! `WireOpResponse` / `WireOpError` message types. The emitted
//! proto string is consumed by the runtime mount's build step
//! (tonic-build / prost-build) to produce the typed Rust
//! service skeleton + client stub.
//!
//! ## RPC shape
//!
//! Every wire op maps to one RPC method. Request-class ops
//! produce a unary RPC; subscribe-class ops produce a
//! server-streaming RPC (the server emits a stream of
//! `WireOpResponse` frames on the subscription channel until
//! the client cancels or the connection closes).
//!
//! Method naming converts the snake-case wire op id to
//! PascalCase: `describe_capabilities` →
//! `DescribeCapabilities`.
//!
//! ## Message shape
//!
//! Op payloads (request parameters + response values) are
//! op-specific JSON; the proto carries them as
//! `google.protobuf.Struct` (the canonical proto representation
//! for arbitrary structured JSON). The per-language gRPC client
//! generators consume the typed wire-schema annotations
//! separately to produce typed bindings; the proto itself stays
//! payload-agnostic so adding a new op does not require a proto
//! migration.
//!
//! ## Listener-agnostic
//!
//! The crate emits the proto file content as a `String`. It
//! does not invoke prost-build / tonic-build, write to disk, or
//! bind a TCP listener. Those concerns belong to the runtime
//! mount.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod emit;
pub mod method_name;
pub mod projection;

pub use emit::{emit_proto, ProtoEmitConfig};
pub use method_name::pascal_case_method_name;
pub use projection::GrpcProjection;

/// The canonical proto package name.
///
/// Generated proto files declare `package evo.v1;` so the
/// runtime tonic build emits Rust types under the
/// `evo::v1` module.
pub const PROTO_PACKAGE: &str = "evo.v1";

/// The canonical service name.
pub const PROTO_SERVICE: &str = "Evo";
