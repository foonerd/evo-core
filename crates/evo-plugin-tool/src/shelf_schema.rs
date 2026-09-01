// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `validate-shelf-schema` subcommand.
//!
//! Walks a schemas tree (the in-tree `dist/catalogue/schemas/`
//! skeleton, the spin-out `evo-catalogue-schemas` repo, or a
//! distribution-installed `/usr/share/evo-catalogue-schemas/`)
//! and validates every per-shelf schema TOML file.
//!
//! Resolution cascade:
//!   1. `--schemas-path` flag.
//!   2. `$EVO_SCHEMAS_DIR` env var.
//!   3. `/usr/share/evo-catalogue-schemas/` (distribution-installed).
//!
//! Bail with a non-zero exit when none of the above resolves.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::Deserialize;

const ENV_SCHEMAS_DIR: &str = "EVO_SCHEMAS_DIR";
const DISTRIBUTION_INSTALL_PATH: &str = "/usr/share/evo-catalogue-schemas";

/// Run the `catalogue validate-shelf-schema` subcommand.
pub fn validate(schemas_path: Option<&Path>) -> Result<(), anyhow::Error> {
    let path = resolve_schemas_path(schemas_path)?;
    if !path.is_dir() {
        return Err(anyhow!(
            "schemas path {} is not a directory",
            path.display()
        ));
    }
    let files = collect_toml_files(&path)?;
    if files.is_empty() {
        println!(
            "no schema files found under {}; tree is empty",
            path.display()
        );
        return Ok(());
    }
    let mut passed = 0_usize;
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for file in &files {
        match validate_one(file) {
            Ok(meta) => {
                println!(
                    "  ok  {}  ({} v{} shape={})",
                    relative_to(&path, file).display(),
                    meta.identity(),
                    meta.schema_version,
                    meta.shape
                );
                passed += 1;
            }
            Err(e) => {
                println!("  FAIL {}: {e}", relative_to(&path, file).display());
                failed.push((file.clone(), e.to_string()));
            }
        }
    }
    println!();
    println!(
        "{} of {} schema files validated cleanly",
        passed,
        files.len()
    );
    if !failed.is_empty() {
        return Err(anyhow!(
            "{} schema file(s) failed validation",
            failed.len()
        ));
    }
    Ok(())
}

fn resolve_schemas_path(
    explicit: Option<&Path>,
) -> Result<PathBuf, anyhow::Error> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Ok(env_path) = env::var(ENV_SCHEMAS_DIR) {
        if !env_path.is_empty() {
            return Ok(PathBuf::from(env_path));
        }
    }
    let distro = PathBuf::from(DISTRIBUTION_INSTALL_PATH);
    if distro.is_dir() {
        return Ok(distro);
    }
    Err(anyhow!(
        "no schemas path resolved. Pass `--schemas-path=<dir>`, set \
         `${ENV_SCHEMAS_DIR}`, or install schemas at \
         {DISTRIBUTION_INSTALL_PATH}"
    ))
}

fn collect_toml_files(root: &Path) -> Result<Vec<PathBuf>, anyhow::Error> {
    let mut out = Vec::new();
    walk_for_toml(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_for_toml(
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), anyhow::Error> {
    let entries = fs::read_dir(dir).with_context(|| {
        format!("reading schemas directory {}", dir.display())
    })?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("iterating {}", dir.display()))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .with_context(|| format!("file_type for {}", path.display()))?;
        if ft.is_dir() {
            walk_for_toml(&path, out)?;
            continue;
        }
        if ft.is_file()
            && path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("toml"))
                .unwrap_or(false)
            && !is_rack_metadata_file(&path)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Rack metadata files (`_rack.toml`) sit alongside shelf
/// schemas in the schemas tree but are NOT shelf schemas —
/// they declare rack-level identity + description and have
/// no `schema_version` / `shape` / `requests` /
/// `course_correct_verbs` fields. The walker skips them so
/// the validator only sees real shelf schemas.
fn is_rack_metadata_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("_rack.toml"))
        .unwrap_or(false)
}

fn relative_to(root: &Path, file: &Path) -> PathBuf {
    file.strip_prefix(root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| file.to_path_buf())
}

fn validate_one(file: &Path) -> Result<ShelfSchemaMeta, anyhow::Error> {
    let body = fs::read_to_string(file)
        .with_context(|| format!("reading {}", file.display()))?;
    let parsed: ShelfSchema = toml::from_str(&body)
        .with_context(|| format!("parsing {}", file.display()))?;
    parsed.validate()?;
    Ok(parsed.meta())
}

/// Per-shelf schema document shape. Minimum-viable validator:
/// every required field must be present, primitive ranges must
/// be sane, and request types must be unique. The authoritative
/// shape lives in the spin-out `evo-catalogue-schemas` repo's
/// CONTRIBUTING guide; this validator is the local-build
/// feedback loop.
///
/// Fields the validator does not currently inspect (descriptions,
/// payload shapes, acceptance details) are still parsed so a
/// malformed file at those nodes refuses at deserialise time.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ShelfSchema {
    schema_version: u32,
    rack: String,
    shelf: String,
    shape: u32,
    /// Shelf shape kind. `"respondent"` (default when
    /// absent) declares `requests`; `"warden"` declares
    /// `course_correct_verbs`. Other values are refused.
    #[serde(default)]
    shape_kind: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    requests: Vec<ShelfRequest>,
    /// Warden-shape: list of `course_correct` verbs the
    /// shelf accepts. Mutually exclusive with `requests` —
    /// validation refuses a schema that declares both.
    #[serde(default)]
    course_correct_verbs: Vec<String>,
    #[serde(default)]
    acceptance: Vec<ShelfAcceptance>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ShelfRequest {
    request_type: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    payload_in: Option<String>,
    #[serde(default)]
    payload_out: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ShelfAcceptance {
    name: String,
    #[serde(default)]
    detail: Option<String>,
}

impl ShelfSchema {
    fn validate(&self) -> Result<(), anyhow::Error> {
        if self.schema_version == 0 {
            return Err(anyhow!("schema_version must be > 0"));
        }
        if self.shape == 0 {
            return Err(anyhow!("shape must be > 0"));
        }
        if self.rack.trim().is_empty() {
            return Err(anyhow!("rack must be a non-empty string"));
        }
        if self.shelf.trim().is_empty() {
            return Err(anyhow!("shelf must be a non-empty string"));
        }

        // Dispatch on shape_kind. The two shape families
        // declare their addressable surface in different
        // fields and are mutually exclusive: a respondent
        // declares `requests`, a warden declares
        // `course_correct_verbs`. A schema declaring both
        // or neither is malformed.
        let shape_kind = self
            .shape_kind
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("respondent");
        match shape_kind {
            "respondent" => {
                if !self.course_correct_verbs.is_empty() {
                    return Err(anyhow!(
                        "respondent-shape schema declares \
                         `course_correct_verbs` (only valid on warden \
                         shape); remove the field or set \
                         `shape_kind = \"warden\"`"
                    ));
                }
                if self.requests.is_empty() {
                    return Err(anyhow!(
                        "respondent-shape schema declares zero request \
                         types; a shelf with no requests is \
                         unaddressable"
                    ));
                }
                let mut seen = std::collections::HashSet::<&str>::new();
                for req in &self.requests {
                    if req.request_type.trim().is_empty() {
                        return Err(anyhow!(
                            "request_type entries must be non-empty"
                        ));
                    }
                    if !seen.insert(req.request_type.as_str()) {
                        return Err(anyhow!(
                            "duplicate request_type {:?}",
                            req.request_type
                        ));
                    }
                }
            }
            "warden" => {
                if !self.requests.is_empty() {
                    return Err(anyhow!(
                        "warden-shape schema declares `requests` (only \
                         valid on respondent shape); remove the field or \
                         set `shape_kind = \"respondent\"`"
                    ));
                }
                if self.course_correct_verbs.is_empty() {
                    return Err(anyhow!(
                        "warden-shape schema declares zero \
                         `course_correct_verbs`; a warden with no verbs \
                         is unaddressable"
                    ));
                }
                let mut seen = std::collections::HashSet::<&str>::new();
                for verb in &self.course_correct_verbs {
                    if verb.trim().is_empty() {
                        return Err(anyhow!(
                            "course_correct_verbs entries must be \
                             non-empty"
                        ));
                    }
                    if !seen.insert(verb.as_str()) {
                        return Err(anyhow!(
                            "duplicate course_correct verb {verb:?}"
                        ));
                    }
                }
            }
            other => {
                return Err(anyhow!(
                    "unknown shape_kind {other:?}; expected \"respondent\" \
                     or \"warden\""
                ));
            }
        }

        for acc in &self.acceptance {
            if acc.name.trim().is_empty() {
                return Err(anyhow!(
                    "acceptance entries must have a non-empty name"
                ));
            }
        }
        Ok(())
    }

    fn meta(&self) -> ShelfSchemaMeta {
        ShelfSchemaMeta {
            rack: self.rack.clone(),
            shelf: self.shelf.clone(),
            schema_version: self.schema_version,
            shape: self.shape,
        }
    }
}

#[derive(Debug)]
struct ShelfSchemaMeta {
    rack: String,
    shelf: String,
    schema_version: u32,
    shape: u32,
}

impl ShelfSchemaMeta {
    fn identity(&self) -> String {
        format!("{}/{}", self.rack, self.shelf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn good_schema() -> &'static str {
        r#"
schema_version = 1
rack = "example"
shelf = "echo"
shape = 1

[[requests]]
request_type = "echo"
"#
    }

    #[test]
    fn validate_passes_on_well_formed_tree() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "example/echo.v1.toml", good_schema());
        validate(Some(tmp.path())).expect("well-formed schema validates");
    }

    #[test]
    fn validate_fails_on_zero_shape() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "example/echo.v1.toml",
            r#"
schema_version = 1
rack = "example"
shelf = "echo"
shape = 0

[[requests]]
request_type = "echo"
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }

    #[test]
    fn validate_fails_on_duplicate_request_types() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "example/echo.v1.toml",
            r#"
schema_version = 1
rack = "example"
shelf = "echo"
shape = 1

[[requests]]
request_type = "echo"

[[requests]]
request_type = "echo"
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }

    #[test]
    fn validate_fails_on_no_requests() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "example/echo.v1.toml",
            r#"
schema_version = 1
rack = "example"
shelf = "echo"
shape = 1
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }

    #[test]
    fn validate_walks_subdirectories() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "org.evoframework/audio/delivery.v1.toml",
            good_schema(),
        );
        write_file(
            tmp.path(),
            "org.evoframework/audio/composition.v1.toml",
            good_schema(),
        );
        write_file(tmp.path(), "example/echo.v1.toml", good_schema());
        validate(Some(tmp.path())).expect("multi-file tree validates");
    }

    #[test]
    fn validate_tolerates_empty_tree() {
        let tmp = TempDir::new().unwrap();
        validate(Some(tmp.path()))
            .expect("empty tree exits 0 with friendly message");
    }

    // ---------------------------------------------------------
    // F5 — schema validator drift coverage. The walker must
    // skip `_rack.toml` files (rack metadata, not shelf
    // schemas), the warden shape must validate when
    // `shape_kind = "warden"` declares `course_correct_verbs`,
    // and the two shape families must be mutually exclusive.
    // ---------------------------------------------------------

    fn warden_schema() -> &'static str {
        r#"
schema_version = 1
rack = "audio"
shelf = "playback"
shape = 1
shape_kind = "warden"

course_correct_verbs = ["play", "pause", "stop"]
"#
    }

    fn rack_metadata() -> &'static str {
        r#"
rack = "audio"
namespace = "org.evoframework"
description = "Brand-neutral audio rack."
"#
    }

    #[test]
    fn validate_passes_on_warden_shape() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "org.evoframework/audio/playback.v1.toml",
            warden_schema(),
        );
        validate(Some(tmp.path())).expect("warden schema must validate");
    }

    #[test]
    fn validate_skips_rack_metadata_file() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "org.evoframework/audio/_rack.toml",
            rack_metadata(),
        );
        // No shelf schemas in tree; _rack.toml is skipped.
        // The walker produces zero files; validate() exits OK
        // with a "tree is empty" message.
        validate(Some(tmp.path())).expect("_rack.toml must be skipped");
    }

    #[test]
    fn validate_walks_rack_alongside_shelf_schemas() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "org.evoframework/audio/_rack.toml",
            rack_metadata(),
        );
        write_file(
            tmp.path(),
            "org.evoframework/audio/playback.v1.toml",
            warden_schema(),
        );
        write_file(
            tmp.path(),
            "org.evoframework/audio/composition.v1.toml",
            good_schema(),
        );
        validate(Some(tmp.path()))
            .expect("rack + warden + respondent tree validates together");
    }

    #[test]
    fn validate_fails_on_warden_with_no_verbs() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "audio/playback.v1.toml",
            r#"
schema_version = 1
rack = "audio"
shelf = "playback"
shape = 1
shape_kind = "warden"
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }

    #[test]
    fn validate_fails_on_warden_with_requests_field() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "audio/playback.v1.toml",
            r#"
schema_version = 1
rack = "audio"
shelf = "playback"
shape = 1
shape_kind = "warden"
course_correct_verbs = ["play"]

[[requests]]
request_type = "play"
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }

    #[test]
    fn validate_fails_on_respondent_with_course_correct_verbs() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "audio/composition.v1.toml",
            r#"
schema_version = 1
rack = "audio"
shelf = "composition"
shape = 1
course_correct_verbs = ["nope"]

[[requests]]
request_type = "select_mode"
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }

    #[test]
    fn validate_fails_on_unknown_shape_kind() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "audio/playback.v1.toml",
            r#"
schema_version = 1
rack = "audio"
shelf = "playback"
shape = 1
shape_kind = "rumpelstiltskin"
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }

    #[test]
    fn validate_fails_on_duplicate_course_correct_verb() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "audio/playback.v1.toml",
            r#"
schema_version = 1
rack = "audio"
shelf = "playback"
shape = 1
shape_kind = "warden"
course_correct_verbs = ["play", "play"]
"#,
        );
        let err = validate(Some(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("schema file(s) failed"));
    }
}
