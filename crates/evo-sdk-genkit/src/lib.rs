// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-sdk-genkit
//!
//! Shared code-emission primitives for the per-language client
//! SDK generators (`evo-sdk-typescript`, `evo-sdk-swift`,
//! `evo-sdk-kotlin`, `evo-sdk-python`, `evo-sdk-rust`).
//!
//! ## Architecture
//!
//! The per-language SDK generators each consume the canonical
//! wire schema and emit a typed client library in their target
//! language. The bulk of the work — walking the schema, grouping
//! ops by domain, classifying each op against its capability
//! and audit scopes, converting snake_case to the target
//! language's identifier conventions — is language-agnostic.
//! `evo-sdk-genkit` packages those primitives once so each
//! per-language generator stays focused on its target's
//! language idioms (`async`/`await` vs callbacks vs futures,
//! `interface` vs `protocol` vs trait, `throw` vs `Result`)
//! rather than re-implementing the schema walk.
//!
//! ## Surface
//!
//! - [`idents`] — identifier-case conversions usable across
//!   every target language: snake_case, camelCase, PascalCase,
//!   SCREAMING_SNAKE_CASE.
//! - [`indent`] — small [`IndentWriter`] helper for writing
//!   indented multi-line code without rolling per-emitter
//!   indent state.
//! - [`op`] — the language-agnostic [`SdkOp`] annotation layer
//!   over [`evo_projection_core::WireOp`]: pre-computes the
//!   per-target method names (snake / camel / Pascal) and the
//!   subscription / step-up classification.
//! - [`grouping`] — `group_schema()` partitions the wire
//!   schema by capability domain so per-language generators
//!   emit one module per domain (`audio.ts`, `plugins.ts`,
//!   `system.py`, etc.).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod grouping;
pub mod idents;
pub mod indent;
pub mod op;

pub use grouping::{group_schema, SdkOpGroup};
pub use idents::{
    to_camel_case, to_pascal_case, to_screaming_snake_case, to_snake_case,
};
pub use indent::IndentWriter;
pub use op::SdkOp;
