// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Emitter configuration.

/// Configuration for the Kotlin SDK generator.
#[derive(Debug, Clone)]
pub struct KotlinConfig {
    /// Indent unit. Kotlin convention is 4 spaces.
    pub indent_unit: String,

    /// Kotlin package name surfaced in every emitted file's
    /// `package` declaration. Defaults to
    /// `"org.evoframework.sdk"`.
    pub package_name: String,
}

impl Default for KotlinConfig {
    fn default() -> Self {
        Self {
            indent_unit: "    ".to_string(),
            package_name: "org.evoframework.sdk".to_string(),
        }
    }
}
