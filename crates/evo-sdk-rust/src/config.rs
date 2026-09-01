// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Emitter configuration.

/// Configuration for the Rust SDK generator.
#[derive(Debug, Clone)]
pub struct RustConfig {
    /// Indent unit. Rust convention is 4 spaces.
    pub indent_unit: String,

    /// Generated crate name surfaced in the lib.rs module
    /// docs. Defaults to `"evo_sdk"`.
    pub crate_name: String,
}

impl Default for RustConfig {
    fn default() -> Self {
        Self {
            indent_unit: "    ".to_string(),
            crate_name: "evo_sdk".to_string(),
        }
    }
}
