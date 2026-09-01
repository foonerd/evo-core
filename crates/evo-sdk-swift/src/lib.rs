// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-sdk-swift
//!
//! Swift client SDK generator.
//!
//! ## Output shape
//!
//! - `EvoClient.swift` — top-level client holding the
//!   `Transport` and one property per capability domain
//! - `Transport.swift` — the `Transport` protocol the runtime
//!   mount implements (HTTP for dispatch, WebSocket for
//!   subscribe — see the evo-projection-rest / -ws crates'
//!   shapes)
//! - `Types.swift` — shared types (`WireOpError`,
//!   `WireOpResult`, `CallOpts`, `SubscribeOpts`)
//! - `Modules/<Group>.swift` — one file per capability domain,
//!   carrying a `<Group>` class with one method per wire op
//!
//! Per-op method shape: `func methodCamel(payload:opts:) async
//! throws -> WireOpResult` for one-shot; `func
//! methodCamel(payload:opts:) -> AsyncThrowingStream<Any,
//! Error>` for subscribe-class ops.
//!
//! Every emitted `.swift` file carries the generated-code
//! header.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod emit;
pub mod rendered;

pub use config::SwiftConfig;
pub use emit::render_sdk;
pub use rendered::{RenderedFile, RenderedSdk};
