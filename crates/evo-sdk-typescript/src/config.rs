// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Emitter configuration.

/// Configuration for the TypeScript SDK generator.
#[derive(Debug, Clone)]
pub struct TypeScriptConfig {
    /// Indent unit. TypeScript style guides typically use
    /// 2 spaces; that's the default. 4 spaces is also valid
    /// for codebases that match Rust / Python conventions.
    pub indent_unit: String,

    /// npm package name surfaced in the generated `index.ts`
    /// header. Defaults to `"@evo/sdk"`.
    pub package_name: String,
}

impl Default for TypeScriptConfig {
    fn default() -> Self {
        Self {
            indent_unit: "  ".to_string(),
            package_name: "@evo/sdk".to_string(),
        }
    }
}
