// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-sdk-python
//!
//! Python client SDK generator.
//!
//! ## Output shape
//!
//! - `__init__.py` — package init re-exporting `EvoClient`
//!   and the shared dataclasses
//! - `client.py` — `EvoClient` class instantiating one
//!   module per capability domain
//! - `transport.py` — `Transport` ABC the runtime mount
//!   implements
//! - `types.py` — `WireOpError`, `WireOpResult`, `CallOpts`,
//!   `SubscribeOpts` dataclasses
//! - `modules/__init__.py` — empty marker so Python treats
//!   `modules/` as a sub-package
//! - `modules/<group>.py` — one file per capability domain
//!
//! Per-op method shape: `async def methodSnake(payload, opts)
//! -> WireOpResult` for one-shot; `async def methodSnake(...)
//! -> AsyncIterator[Any]` (`async def` generator with `yield`)
//! for subscribe-class ops.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod emit;
pub mod rendered;

pub use config::PythonConfig;
pub use emit::render_sdk;
pub use rendered::{RenderedFile, RenderedSdk};
