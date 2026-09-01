// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Output types.

/// One emitted Kotlin file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    /// Relative path under the SDK source root.
    pub path: String,
    /// File content. Carries the generated-code header on
    /// the first line.
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

/// The full rendered Kotlin SDK.
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
    fn rendered_file_holds_data() {
        let f = RenderedFile::new("EvoClient.kt", "x");
        assert_eq!(f.path, "EvoClient.kt");
    }

    #[test]
    fn rendered_sdk_find_returns_match() {
        let sdk = RenderedSdk {
            files: vec![RenderedFile::new("a.kt", "a")],
        };
        assert!(sdk.find("a.kt").is_some());
        assert!(sdk.find("missing.kt").is_none());
    }

    #[test]
    fn rendered_sdk_reports_count() {
        let sdk = RenderedSdk {
            files: vec![
                RenderedFile::new("a.kt", "a"),
                RenderedFile::new("b.kt", "b"),
            ],
        };
        assert_eq!(sdk.file_count(), 2);
    }
}
