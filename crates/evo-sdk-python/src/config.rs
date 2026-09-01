// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Emitter configuration.

/// Configuration for the Python SDK generator.
#[derive(Debug, Clone)]
pub struct PythonConfig {
    /// Indent unit. PEP 8 specifies 4 spaces.
    pub indent_unit: String,

    /// Python package name surfaced in the generated
    /// `__init__.py` docstring. Defaults to `"evo_sdk"`.
    pub package_name: String,
}

impl Default for PythonConfig {
    fn default() -> Self {
        Self {
            indent_unit: "    ".to_string(),
            package_name: "evo_sdk".to_string(),
        }
    }
}
