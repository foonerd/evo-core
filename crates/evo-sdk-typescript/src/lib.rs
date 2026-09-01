// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-sdk-typescript
//!
//! TypeScript client SDK generator.
//!
//! ## Architecture
//!
//! Walks the canonical wire schema (a slice of `WireOp` from
//! `evo::projection_schema::canonical_schema()`), groups the
//! ops by capability domain via
//! `evo_sdk_genkit::group_schema()`, and emits a multi-file
//! TypeScript SDK ready for npm publication.
//!
//! ## Output shape
//!
//! The rendered SDK is a [`RenderedSdk`] carrying multiple
//! [`RenderedFile`]s with relative paths:
//!
//! - `index.ts` — top-level re-exports of `EvoClient` and the
//!   shared types
//! - `client.ts` — `EvoClient` class with one property per
//!   capability domain, each holding an instance of the
//!   corresponding module class
//! - `types.ts` — shared types: `WireOpError`, `WireOpResult`,
//!   `CallOpts`, `SubscribeOpts`
//! - `transport.ts` — HTTP fetch + WebSocket transport
//!   abstraction (the implementation stub the runtime mount
//!   completes per its dispatch wiring)
//! - `modules/<group>.ts` — one file per capability domain,
//!   carrying a `<GroupPascal>Module` class with one method
//!   per wire op in the group
//!
//! Every emitted `.ts` file carries the generated-code header
//! from `evo-projection-core` so a CI pre-commit gate refuses
//! hand-edits to the persisted files.
//!
//! ## Method shape
//!
//! Per-op method shape on the module classes:
//!
//! ```typescript
//! /**
//!  * <summary>
//!  *
//!  * Capability: <scope>
//!  * Step-up required: yes/no
//!  */
//! public async <methodCamel>(
//!     payload?: Record<string, unknown>,
//!     opts?: CallOpts,
//! ): Promise<WireOpResult> {
//!     return this.transport.dispatch('<op_id>', payload ?? {}, opts);
//! }
//! ```
//!
//! Subscription ops emit an `AsyncIterable` shape:
//!
//! ```typescript
//! public async *<methodCamel>(
//!     payload?: Record<string, unknown>,
//!     opts?: SubscribeOpts,
//! ): AsyncIterable<unknown> {
//!     yield* this.transport.subscribe('<op_id>', payload ?? {}, opts);
//! }
//! ```
//!
//! Generic `Record<string, unknown>` payloads keep the SDK
//! usable today; per-op typed payload interfaces require a
//! separate payload-type annotation pass on the wire schema
//! and land as their own chunk.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod emit;
pub mod rendered;

pub use config::TypeScriptConfig;
pub use emit::render_sdk;
pub use rendered::{RenderedFile, RenderedSdk};
