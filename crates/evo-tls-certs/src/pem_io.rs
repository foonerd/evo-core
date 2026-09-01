// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! PEM file I/O helpers.
//!
//! Wraps `std::fs` with the framework's error taxonomy so
//! callers don't repeat `map_err` boilerplate at every call
//! site.

use crate::error::CertError;
use std::fs;
use std::path::Path;

/// Read a PEM file into a String.
pub fn read_pem_file(path: impl AsRef<Path>) -> Result<String, CertError> {
    let path = path.as_ref();
    fs::read_to_string(path).map_err(|source| CertError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Write PEM content to a file. Creates the parent directory
/// if missing (`mkdir -p` semantics). Truncates an existing
/// file; the operator-owned key + cert paths are atomic from
/// the framework's view (a partial write is recoverable).
pub fn write_pem_file(
    path: impl AsRef<Path>,
    content: &str,
) -> Result<(), CertError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| CertError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
    }
    fs::write(path, content).map_err(|source| CertError::Io {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_round_trips_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.pem");
        write_pem_file(&path, "PEM CONTENT").unwrap();
        let back = read_pem_file(&path).unwrap();
        assert_eq!(back, "PEM CONTENT");
    }

    #[test]
    fn write_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("test.pem");
        write_pem_file(&path, "X").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn read_nonexistent_file_returns_io_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.pem");
        let result = read_pem_file(&path);
        assert!(matches!(result, Err(CertError::Io { .. })));
    }

    #[test]
    fn write_overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("over.pem");
        write_pem_file(&path, "first").unwrap();
        write_pem_file(&path, "second").unwrap();
        let back = read_pem_file(&path).unwrap();
        assert_eq!(back, "second");
    }
}
