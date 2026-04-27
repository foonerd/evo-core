//! `catalogue` — operations on catalogue documents.
//!
//! Subcommands:
//!
//! - `lint` — parse and validate a catalogue document at the given
//!   path. With `--schema-version N` the lint additionally pins the
//!   document's `schema_version` to N (an authoring-time discipline so
//!   distribution authors can refuse a fixture-update slip-through).
//!   Without the flag the document's own `schema_version` is used.

use anyhow::{anyhow, Context};
use evo::catalogue::{Catalogue, CATALOGUE_SCHEMA_MAX, CATALOGUE_SCHEMA_MIN};
use std::path::Path;

/// Parse and validate a catalogue at `path`.
///
/// On success, prints `OK: schema_version = N` to stderr and returns
/// `Ok(())`. On failure, returns the parse / validation error so the
/// caller can map it to a non-zero process exit code.
///
/// `pin_schema_version`: when `Some(N)`, the document's
/// `schema_version` must equal `N` exactly (in addition to lying in
/// the steward's supported `[MIN, MAX]` range that
/// [`Catalogue::validate`] already enforces). The flag is the
/// authoring-time discipline a distribution uses to assert the
/// version the document is authored against. When `None`, no
/// extra pin is applied.
pub fn lint(
    path: &Path,
    pin_schema_version: Option<u32>,
) -> Result<(), anyhow::Error> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read catalogue {}", path.display()))?;
    let cat = Catalogue::from_toml(&text).with_context(|| {
        format!("parse / validate catalogue {}", path.display())
    })?;

    if let Some(want) = pin_schema_version {
        if cat.schema_version != want {
            return Err(anyhow!(
                "catalogue {} declares schema_version = {}, but \
                 --schema-version pins it at {}",
                path.display(),
                cat.schema_version,
                want
            ));
        }
    }

    eprintln!(
        "OK: {} (schema_version = {}; supported range [{}, {}])",
        path.display(),
        cat.schema_version,
        CATALOGUE_SCHEMA_MIN,
        CATALOGUE_SCHEMA_MAX,
    );
    Ok(())
}
