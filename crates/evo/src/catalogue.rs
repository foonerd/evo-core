//! The catalogue: racks, shelves, and their declarations.
//!
//! The catalogue is TOML-as-data per `CONCEPT.md` section 6. The steward
//! reads it at startup, validates it, and exposes it to the admission
//! engine. Plugins targeting a shelf must name it by fully-qualified name
//! (`<rack>.<shelf>`).
//!
//! For v0 the schema is deliberately minimal:
//!
//! - Racks with name, family, kinds, charter, and a list of shelves
//!   with names and shape versions.
//! - Relation predicates declared at top level, per `RELATIONS.md`
//!   section 3: predicate name, description, source and target type
//!   constraints, cardinalities, and optional inverse predicate name.
//!
//! Future passes extend this as shelf shapes grow schema and as
//! subject-type declarations are added.

use crate::error::StewardError;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A catalogue: the full set of racks and relation predicates declared
/// by the distribution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Catalogue {
    /// Racks declared in this catalogue. Order is not semantically
    /// significant but is preserved for diagnostic stability.
    #[serde(default)]
    pub racks: Vec<Rack>,
    /// Relation predicates declared in this catalogue. The set of
    /// predicates plugins may assert between subjects, per
    /// `RELATIONS.md` section 3.
    #[serde(default, rename = "relation")]
    pub relations: Vec<RelationPredicate>,
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

/// Cardinality constraint on one side of a relation predicate, per
/// `RELATIONS.md` section 3.2. Cardinality violations emit warnings
/// rather than rejecting assertions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// Exactly one subject on this side.
    ExactlyOne,
    /// At most one subject on this side; zero is allowed.
    AtMostOne,
    /// At least one subject on this side; upper bound unconstrained.
    AtLeastOne,
    /// No constraint.
    Many,
}

impl Default for Cardinality {
    fn default() -> Self {
        Self::Many
    }
}

/// A constraint on which subject types may appear on one side of a
/// relation. Accepts either a single type name (e.g. `"track"`) or a
/// list of type names (e.g. `["track", "podcast_episode"]`). The
/// special value `"*"` matches any type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum TypeConstraint {
    /// Single type name, or `"*"` for any.
    Single(String),
    /// One-of: any of the listed types.
    Multiple(Vec<String>),
}

impl TypeConstraint {
    /// True if the given subject type satisfies this constraint.
    ///
    /// `"*"` matches any type; otherwise the type must appear in the
    /// constraint's type set.
    pub fn accepts(&self, subject_type: &str) -> bool {
        match self {
            Self::Single(t) => t == "*" || t == subject_type,
            Self::Multiple(ts) => ts.iter().any(|t| t == subject_type),
        }
    }
}

/// A relation predicate declared in the catalogue, per `RELATIONS.md`
/// section 3.
///
/// Predicates are named directed edges between subjects. The catalogue
/// constrains which subject types may appear on each side and what
/// cardinality is expected. An optional `inverse` names the reverse
/// predicate consumers may use to walk backwards along this relation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationPredicate {
    /// Predicate name. Lowercase, snake_case, no dots.
    pub predicate: String,
    /// One-sentence description of what the predicate means.
    #[serde(default)]
    pub description: String,
    /// Type constraint on the source side.
    pub source_type: TypeConstraint,
    /// Type constraint on the target side.
    pub target_type: TypeConstraint,
    /// Cardinality on the source side (how many source subjects a
    /// single target may have via this predicate). Defaults to
    /// [`Cardinality::Many`].
    #[serde(default)]
    pub source_cardinality: Cardinality,
    /// Cardinality on the target side (how many target subjects a
    /// single source may have via this predicate). Defaults to
    /// [`Cardinality::Many`].
    #[serde(default)]
    pub target_cardinality: Cardinality,
    /// Optional inverse predicate name. If present, the named
    /// predicate MUST also be declared in the catalogue, with source
    /// and target types swapped, and its own `inverse` pointing back
    /// to this one.
    #[serde(default)]
    pub inverse: Option<String>,
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

    /// Look up a relation predicate by name.
    ///
    /// Returns `None` if no such predicate is declared.
    pub fn find_predicate(&self, name: &str) -> Option<&RelationPredicate> {
        self.relations.iter().find(|p| p.predicate == name)
    }

    /// Validate structural invariants: no duplicate rack names, no
    /// duplicate shelf names within a rack, non-empty names and
    /// charters, no duplicate relation predicates, non-empty predicate
    /// fields.
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

        let mut pred_names = std::collections::HashSet::new();
        for pred in &self.relations {
            if pred.predicate.is_empty() {
                return Err(StewardError::Catalogue(
                    "relation predicate with empty name".into(),
                ));
            }
            if pred.predicate.contains('.') {
                return Err(StewardError::Catalogue(format!(
                    "predicate name must not contain '.': {}",
                    pred.predicate
                )));
            }
            if !pred_names.insert(&pred.predicate) {
                return Err(StewardError::Catalogue(format!(
                    "duplicate relation predicate: {}",
                    pred.predicate
                )));
            }
            if let TypeConstraint::Multiple(ts) = &pred.source_type {
                if ts.is_empty() {
                    return Err(StewardError::Catalogue(format!(
                        "predicate {} has empty source_type list",
                        pred.predicate
                    )));
                }
            }
            if let TypeConstraint::Multiple(ts) = &pred.target_type {
                if ts.is_empty() {
                    return Err(StewardError::Catalogue(format!(
                        "predicate {} has empty target_type list",
                        pred.predicate
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
        assert!(c.relations.is_empty());
    }

    #[test]
    fn parses_relation_predicate_with_single_types() {
        let toml = r#"
[[relation]]
predicate = "album_of"
description = "track belongs to album"
source_type = "track"
target_type = "album"
source_cardinality = "at_most_one"
target_cardinality = "many"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "album"
target_type = "track"
source_cardinality = "many"
target_cardinality = "at_most_one"
inverse = "album_of"
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        assert_eq!(c.relations.len(), 2);
        let album_of = c.find_predicate("album_of").unwrap();
        assert_eq!(album_of.inverse.as_deref(), Some("tracks_of"));
        assert_eq!(album_of.source_cardinality, Cardinality::AtMostOne);
    }

    #[test]
    fn parses_relation_predicate_with_multiple_types() {
        let toml = r#"
[[relation]]
predicate = "contained_in"
source_type = "*"
target_type = ["folder", "archive"]
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        let p = c.find_predicate("contained_in").unwrap();
        match &p.source_type {
            TypeConstraint::Single(s) => assert_eq!(s, "*"),
            _ => panic!("expected Single"),
        }
        match &p.target_type {
            TypeConstraint::Multiple(ts) => {
                assert_eq!(ts, &vec!["folder".to_string(), "archive".to_string()]);
            }
            _ => panic!("expected Multiple"),
        }
    }

    #[test]
    fn type_constraint_accepts_any_with_star() {
        let c = TypeConstraint::Single("*".into());
        assert!(c.accepts("anything"));
        assert!(c.accepts("track"));
    }

    #[test]
    fn type_constraint_accepts_exact_single() {
        let c = TypeConstraint::Single("track".into());
        assert!(c.accepts("track"));
        assert!(!c.accepts("album"));
    }

    #[test]
    fn type_constraint_accepts_any_of_multiple() {
        let c =
            TypeConstraint::Multiple(vec!["track".into(), "podcast".into()]);
        assert!(c.accepts("track"));
        assert!(c.accepts("podcast"));
        assert!(!c.accepts("album"));
    }

    #[test]
    fn cardinality_defaults_to_many() {
        let toml = r#"
[[relation]]
predicate = "p"
source_type = "t"
target_type = "t"
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        let p = c.find_predicate("p").unwrap();
        assert_eq!(p.source_cardinality, Cardinality::Many);
        assert_eq!(p.target_cardinality, Cardinality::Many);
    }

    #[test]
    fn rejects_duplicate_predicates() {
        let toml = r#"
[[relation]]
predicate = "p"
source_type = "a"
target_type = "b"

[[relation]]
predicate = "p"
source_type = "c"
target_type = "d"
"#;
        let r = Catalogue::from_toml(toml);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }

    #[test]
    fn rejects_predicate_with_dot() {
        let toml = r#"
[[relation]]
predicate = "a.b"
source_type = "x"
target_type = "y"
"#;
        let r = Catalogue::from_toml(toml);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }

    #[test]
    fn rejects_predicate_with_empty_name() {
        let toml = r#"
[[relation]]
predicate = ""
source_type = "x"
target_type = "y"
"#;
        let r = Catalogue::from_toml(toml);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }

    #[test]
    fn rejects_predicate_with_empty_multiple_types() {
        let toml = r#"
[[relation]]
predicate = "p"
source_type = []
target_type = "t"
"#;
        let r = Catalogue::from_toml(toml);
        assert!(matches!(r, Err(StewardError::Catalogue(_))));
    }
}
