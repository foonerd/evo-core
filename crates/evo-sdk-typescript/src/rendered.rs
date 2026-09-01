// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Output types: [`RenderedSdk`] and [`RenderedFile`].

/// One emitted file in the rendered SDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    /// Relative path under the SDK package root (e.g.
    /// `"index.ts"`, `"modules/plugins.ts"`).
    pub path: String,

    /// File content. Carries the generated-code header on the
    /// first line.
    pub content: String,
}

impl RenderedFile {
    /// Construct a `RenderedFile` from a path and content.
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

/// The full rendered SDK: a flat list of files the runtime
/// mount writes to disk under the SDK package root.
///
/// File order in the vector matches the natural read order
/// (`index.ts` first, then the shared `client.ts` / `types.ts`
/// / `transport.ts`, then `modules/*` in alphabetical order
/// with `discovery` first). The order is informative for
/// generated documentation and stable across regenerations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSdk {
    /// The emitted files.
    pub files: Vec<RenderedFile>,
}

impl RenderedSdk {
    /// Total number of files in this SDK.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Find a file by exact path.
    pub fn find(&self, path: &str) -> Option<&RenderedFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_file_holds_path_and_content() {
        let f = RenderedFile::new("index.ts", "export * from './client';");
        assert_eq!(f.path, "index.ts");
        assert!(f.content.starts_with("export"));
    }

    #[test]
    fn rendered_sdk_reports_file_count() {
        let sdk = RenderedSdk {
            files: vec![
                RenderedFile::new("a.ts", "// a"),
                RenderedFile::new("b.ts", "// b"),
            ],
        };
        assert_eq!(sdk.file_count(), 2);
    }

    #[test]
    fn rendered_sdk_find_by_path() {
        let sdk = RenderedSdk {
            files: vec![
                RenderedFile::new("index.ts", "// idx"),
                RenderedFile::new("modules/plugins.ts", "// p"),
            ],
        };
        assert!(sdk.find("index.ts").is_some());
        assert!(sdk.find("modules/plugins.ts").is_some());
        assert!(sdk.find("nope.ts").is_none());
    }
}
