// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Output types: [`RenderedSdk`] and [`RenderedFile`].

/// One emitted Swift source file in the rendered SDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    /// Relative path under the SDK source root (e.g.
    /// `"EvoClient.swift"`, `"Modules/Plugins.swift"`).
    pub path: String,

    /// File content. Carries the generated-code header on
    /// the first line.
    pub content: String,
}

impl RenderedFile {
    /// Construct a `RenderedFile`.
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

/// The full rendered Swift SDK as a flat file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSdk {
    /// The emitted files.
    pub files: Vec<RenderedFile>,
}

impl RenderedSdk {
    /// Total file count.
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
        let f = RenderedFile::new("EvoClient.swift", "// test");
        assert_eq!(f.path, "EvoClient.swift");
    }

    #[test]
    fn rendered_sdk_find_returns_match() {
        let sdk = RenderedSdk {
            files: vec![
                RenderedFile::new("a.swift", "a"),
                RenderedFile::new("b.swift", "b"),
            ],
        };
        assert!(sdk.find("a.swift").is_some());
        assert!(sdk.find("nope.swift").is_none());
    }

    #[test]
    fn rendered_sdk_reports_file_count() {
        let sdk = RenderedSdk {
            files: vec![
                RenderedFile::new("a.swift", "a"),
                RenderedFile::new("b.swift", "b"),
                RenderedFile::new("c.swift", "c"),
            ],
        };
        assert_eq!(sdk.file_count(), 3);
    }
}
