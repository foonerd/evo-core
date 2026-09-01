// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Emitter configuration.

/// Configuration for the Swift SDK generator.
#[derive(Debug, Clone)]
pub struct SwiftConfig {
    /// Indent unit. Swift convention is 4 spaces.
    pub indent_unit: String,

    /// Swift module name surfaced in the generated
    /// `EvoClient.swift` header. Defaults to `"EvoSdk"`.
    pub module_name: String,
}

impl Default for SwiftConfig {
    fn default() -> Self {
        Self {
            indent_unit: "    ".to_string(),
            module_name: "EvoSdk".to_string(),
        }
    }
}
