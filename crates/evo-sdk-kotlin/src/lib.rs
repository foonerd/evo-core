// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-sdk-kotlin
//!
//! Kotlin client SDK generator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod emit;
pub mod rendered;

pub use config::KotlinConfig;
pub use emit::render_sdk;
pub use rendered::{RenderedFile, RenderedSdk};
