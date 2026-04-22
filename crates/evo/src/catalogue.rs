//! The catalogue: racks, shelves, and their declarations.
//!
//! The catalogue is TOML-as-data per `CONCEPT.md` section 6. The steward
//! reads it at startup, validates it, and exposes it to the admission
//! engine. Plugins targeting a shelf must name it by fully-qualified name
//! (`<rack>.<shelf>`).
//!
//! For v0 the schema is deliberately minimal: racks with names, family,
//! kinds, charter, and a list of shelves with names and shape versions.
//! Future passes extend this as shelf shapes grow schema.

use crate::error::StewardError;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A catalogue: the full set of racks declared by the distribution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Catalogue {
    /// Racks declared in this catalogue. Order is not semantically
    /// significant but is preserved for diagnostic stability.
    #[serde(default)]
    pub racks: Vec<Rack>,
}

/// A rack: a concern, per `CONCEPT.md` section 4.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rack {
    /// Name of the rack (e.g. `audio`, `example`). Lowercase, no dots.
    pub name: String,
    /// Rack family: `domain`, `coordination`, or `infrastructure`.
    pub family: String,
    /// Rack kinds: any of `producer`, `transformer`, `presenter`,
    /// `registrar`. A rack may have more than one kind.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// One-sentence description of what this rack does.
    pub charter: String,
    /// Shelves declared in this rack.
    #[serde(default)]
    pub shelves: Vec<Shelf>,
}

/// A shelf: a typed opening within a rack that plugins stock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shelf {
    /// Name of the shelf within its rack (e.g. `echo`, `providers`).
    /// Lowercase, no dots.
    pub name: String,
    /// Shelf shape version. Plugins declare which shape they satisfy.
    pub shape: u32,
    /// One-sentence description.
    #[serde(default)]
    pub description: String,
}

impl Catalogue {
    /// Load a catalogue from a TOML file.
    pub fn load(path: &Path) -> Result<Self, StewardError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            StewardError::io(format!("reading catalogue {}", path.display()), e)
        })?;
        Self::from_toml(&content)
            .map_err(|e| match e {
                StewardError::Toml { source, .. } => StewardError::toml(
                    format!("{}", path.display()),
                    source,
                ),
                other => other,
            })
    }

    /// Parse a catalogue from a TOML string.
    pub fn from_toml(input: &str) -> Result<Self, StewardError> {
        let c: Self = toml::from_str(input)
            .map_err(|e| StewardError::toml("catalogue", e))?;
        c.validate()?;
        Ok(c)
    }

    /// Look up a shelf by fully qualified name (`<rack>.<shelf>`).
    ///
    /// Returns `None` if either the rack or the shelf does not exist, or
    /// if the name is malformed (missing the dot separator).
    pub fn find_shelf(&self, qualified_name: &str) -> Option<&Shelf> {
        let (rack_name, shelf_name) = qualified_name.split_once('.')?;
        self.racks
            .iter()
            .find(|r| r.name == rack_name)?
            .shelves
            .iter()
            .find(|s| s.name == shelf_name)
    }

    /// Validate structural invariants: no duplicate rack names, no
    /// duplicate shelf names within a rack, non-empty names and charters.
    pub fn validate(&self) -> Result<(), StewardError> {
        let mut rack_names = std::collections::HashSet::new();
        for rack in &self.racks {
            if rack.name.is_empty() {
                return Err(StewardError::Catalogue(
                    "rack with empty name".into(),
                ));
            }
            if rack.name.contains('.') {
                return Err(StewardError::Catalogue(format!(
                    "rack name must not contain '.': {}",
                    rack.name
                )));
            }
            if rack.charter.is_empty() {
                return Err(StewardError::Catalogue(format!(
                    "rack {} has empty charter",
                    rack.name
                )));
            }
            if !rack_names.insert(&rack.name) {
                return Err(StewardError::Catalogue(format!(
                    "duplicate rack name: {}",
                    rack.name
                )));
            }

            let mut shelf_names = std::collections::HashSet::new();
            for shelf in &rack.shelves {
                if shelf.name.is_empty() {
                    return Err(StewardError::Catalogue(format!(
                        "shelf with empty name in rack {}",
                        rack.name
                    )));
                }
                if shelf.name.contains('.') {
                    return Err(StewardError::Catalogue(format!(
                        "shelf name must not contain '.': {}.{}",
                        rack.name, shelf.name
                    )));
                }
                if !shelf_names.insert(&shelf.name) {
                    return Err(StewardError::Catalogue(format!(
                        "duplicate shelf name {} in rack {}",
                        shelf.name, rack.name
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Example rack."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echo test shelf."
"#;

    #[test]
    fn parses_minimal_catalogue() {
        let c = Catalogue::from_toml(MINIMAL).unwrap();
        assert_eq!(c.racks.len(), 1);
        assert_eq!(c.racks[0].name, "example");
        assert_eq!(c.racks[0].shelves.len(), 1);
        assert_eq!(c.racks[0].shelves[0].name, "echo");
        assert_eq!(c.racks[0].shelves[0].shape, 1);
    }

    #[test]
    fn finds_shelf_by_qualified_name() {
        let c = Catalogue::from_toml(MINIMAL).unwrap();
        let s = c.find_shelf("example.echo").unwrap();
        assert_eq!(s.name, "echo");
    }

    #[test]
    fn missing_shelf_returns_none() {
        let c = Catalogue::from_toml(MINIMAL).unwrap();
        assert!(c.find_shelf("example.missing").is_none());
        assert!(c.find_shelf("missing.echo").is_none());
        assert!(c.find_shelf("no-dot-here").is_none());
    }

    #[test]
    fn rejects_duplicate_rack_names() {
        let bad = r#"
[[racks]]
name = "a"
family = "domain"
charter = "one"

[[racks]]
name = "a"
family = "domain"
charter = "two"
"#;
        let r = Catalogue::from_toml(bad);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }

    #[test]
    fn rejects_duplicate_shelf_names() {
        let bad = r#"
[[racks]]
name = "a"
family = "domain"
charter = "rack"

[[racks.shelves]]
name = "s"
shape = 1

[[racks.shelves]]
name = "s"
shape = 2
"#;
        let r = Catalogue::from_toml(bad);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }

    #[test]
    fn rejects_empty_rack_name() {
        let bad = r#"
[[racks]]
name = ""
family = "domain"
charter = "x"
"#;
        let r = Catalogue::from_toml(bad);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }

    #[test]
    fn rejects_rack_name_with_dot() {
        let bad = r#"
[[racks]]
name = "a.b"
family = "domain"
charter = "x"
"#;
        let r = Catalogue::from_toml(bad);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }

    #[test]
    fn empty_catalogue_is_valid() {
        let c = Catalogue::from_toml("").unwrap();
        assert!(c.racks.is_empty());
    }
}
