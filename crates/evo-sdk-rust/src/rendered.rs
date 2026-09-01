// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Output types.

/// One emitted Rust file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    /// Relative path under the SDK crate root.
    pub path: String,
    /// File content; carries the generated-code header.
    pub content: String,
}

impl RenderedFile {
    /// Construct.
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

/// The full rendered Rust SDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSdk {
    /// Emitted files.
    pub files: Vec<RenderedFile>,
}

impl RenderedSdk {
    /// File count.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Find by path.
    pub fn find(&self, path: &str) -> Option<&RenderedFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_data() {
        let f = RenderedFile::new("lib.rs", "x");
        assert_eq!(f.path, "lib.rs");
    }

    #[test]
    fn find_returns_match() {
        let sdk = RenderedSdk {
            files: vec![RenderedFile::new("a.rs", "a")],
        };
        assert!(sdk.find("a.rs").is_some());
        assert!(sdk.find("missing.rs").is_none());
    }

    #[test]
    fn reports_count() {
        let sdk = RenderedSdk {
            files: vec![
                RenderedFile::new("a.rs", "a"),
                RenderedFile::new("b.rs", "b"),
            ],
        };
        assert_eq!(sdk.file_count(), 2);
    }
}
