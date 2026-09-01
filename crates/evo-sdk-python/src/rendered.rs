// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Output types.

/// One emitted Python file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    /// Relative path under the package root.
    pub path: String,
    /// File content. Carries the generated-code header.
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

/// The full rendered Python SDK.
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
        let f = RenderedFile::new("__init__.py", "x");
        assert_eq!(f.path, "__init__.py");
    }

    #[test]
    fn rendered_sdk_find_works() {
        let sdk = RenderedSdk {
            files: vec![RenderedFile::new("a.py", "a")],
        };
        assert!(sdk.find("a.py").is_some());
        assert!(sdk.find("missing.py").is_none());
    }

    #[test]
    fn rendered_sdk_reports_count() {
        let sdk = RenderedSdk {
            files: vec![
                RenderedFile::new("a.py", "a"),
                RenderedFile::new("b.py", "b"),
            ],
        };
        assert_eq!(sdk.file_count(), 2);
    }
}
