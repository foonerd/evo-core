//! The catalogue: racks, shelves, subject types, and relation
//! predicate declarations.
//!
//! The catalogue is TOML-as-data per `CONCEPT.md` section 6. The steward
//! reads it at startup, validates it, and exposes it to the admission
//! engine. Plugins targeting a shelf must name it by fully-qualified name
//! (`<rack>.<shelf>`).
//!
//! The catalogue declares four things (`CATALOGUE.md` section 2):
//!
//! - Racks with name, family, kinds, charter, and a list of shelves
//!   with names and shape versions.
//! - Subject types declared at top level (`SUBJECTS.md` section 4):
//!   the fabric-level vocabulary of kinds-of-things the catalogue has
//!   opinions about.
//! - Relation predicates declared at top level (`RELATIONS.md` section
//!   3): predicate name, description, source and target type
//!   constraints, cardinalities, and optional inverse predicate name.
//!
//! The fourth concern traditionally listed alongside these - shelf
//! shapes - is carried on individual shelves rather than as a
//! separate top-level section.
//!
//! ## Validation
//!
//! [`Catalogue::validate`] enforces structural invariants at load
//! time:
//!
//! - Rack and shelf naming (non-empty, no dots, unique within scope).
//! - Subject-type naming and uniqueness.
//! - Relation-predicate naming and uniqueness.
//! - Relation-predicate `source_type` and `target_type` references
//!   refer to declared subject types or the wildcard `"*"`.
//! - Inverse-predicate consistency: a declared inverse exists, its
//!   source/target types are swapped with respect to the referring
//!   predicate, and its own `inverse` points back.
//!
//! Runtime enforcement that consumes the validated catalogue lives
//! in [`crate::context`]: predicate existence and relation
//! type-constraint checks on assert, subject-type existence on
//! announce.

use crate::error::StewardError;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Lowest catalogue schema version this steward parses.
///
/// A catalogue document declaring `schema_version = N` is admissible
/// only when `CATALOGUE_SCHEMA_MIN <= N <= CATALOGUE_SCHEMA_MAX`.
/// Below this is a refusal at parse time; the steward will not
/// auto-upgrade older documents.
pub const CATALOGUE_SCHEMA_MIN: u32 = 1;

/// Highest catalogue schema version this steward parses.
///
/// Schema bumps are integer-valued; a breaking grammar change
/// (removed field, newly-required field, type narrowed, semantic
/// shift) requires incrementing this constant. Additive changes
/// (new optional field, new optional section) stay within the
/// current schema version because parsers tolerate unknown fields.
pub const CATALOGUE_SCHEMA_MAX: u32 = 1;

/// A catalogue: the full set of racks, subject types, and relation
/// predicates declared by the distribution.
///
/// Catalogue documents carry a top-level `schema_version` integer.
/// This field is required at parse time; a document without it is
/// rejected (see [`Catalogue::from_toml`]). The integer indexes a
/// versioned grammar: distributions know which catalogue grammar
/// they author against, and the steward declares the supported
/// range via [`CATALOGUE_SCHEMA_MIN`] / [`CATALOGUE_SCHEMA_MAX`].
/// Migration is forward-only — the steward never silently rewrites
/// an operator-edited catalogue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Catalogue {
    /// Catalogue document schema version. Required at parse time
    /// (no default); a missing field rejects with a structured
    /// `Misconfiguration` error pointing at the catalogue path.
    /// Must lie in `[CATALOGUE_SCHEMA_MIN, CATALOGUE_SCHEMA_MAX]`
    /// at validation time. Distinct from per-shelf [`Shelf::shape`],
    /// which versions a shelf's plugin contract.
    pub schema_version: u32,
    /// Racks declared in this catalogue. Order is not semantically
    /// significant but is preserved for diagnostic stability.
    #[serde(default)]
    pub racks: Vec<Rack>,
    /// Subject types declared in this catalogue, per `SUBJECTS.md`
    /// section 4. The vocabulary of kinds-of-things the fabric has
    /// opinions about; plugins may only announce subjects whose type
    /// appears here, enforced at the wiring layer by
    /// [`crate::context::RegistrySubjectAnnouncer`].
    #[serde(default, rename = "subjects")]
    pub subjects: Vec<SubjectType>,
    /// Relation predicates declared in this catalogue. The set of
    /// predicates plugins may assert between subjects, per
    /// `RELATIONS.md` section 3.
    #[serde(default, rename = "relation")]
    pub relations: Vec<RelationPredicate>,
}

impl Default for Catalogue {
    fn default() -> Self {
        Self {
            schema_version: CATALOGUE_SCHEMA_MIN,
            racks: Vec::new(),
            subjects: Vec::new(),
            relations: Vec::new(),
        }
    }
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

/// A subject type: a catalogue-declared kind-of-thing the fabric
/// has opinions about, per `SUBJECTS.md` section 4.
///
/// Subject types are fabric-level vocabulary. A distribution decides
/// what kinds of things the fabric has opinions about; plugins
/// contribute knowledge about those things but do not introduce new
/// kinds. Adding a new subject type is a catalogue edit, not a code
/// change or a manifest change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubjectType {
    /// Subject-type name. Lowercase, snake_case, no dots. Globally
    /// unique within a catalogue.
    pub name: String,
    /// One-sentence description of what this type represents.
    #[serde(default)]
    pub description: String,
}

/// Cardinality constraint on one side of a relation predicate, per
/// `RELATIONS.md` section 3.2. Cardinality violations emit warnings
/// rather than rejecting assertions.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// Exactly one subject on this side.
    ExactlyOne,
    /// At most one subject on this side; zero is allowed.
    AtMostOne,
    /// At least one subject on this side; upper bound unconstrained.
    AtLeastOne,
    /// No constraint.
    #[default]
    Many,
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
        Self::from_toml(&content).map_err(|e| match e {
            StewardError::Toml { source, .. } => {
                StewardError::toml(format!("{}", path.display()), source)
            }
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

    /// Look up a subject type by name.
    ///
    /// Returns `None` if no such type is declared in this catalogue.
    /// Used by [`crate::context::RegistrySubjectAnnouncer`] to reject
    /// announcements of undeclared subject types at the wiring layer,
    /// and by [`Self::validate`] to check that every relation-predicate
    /// `source_type` / `target_type` reference names a declared type.
    pub fn find_subject_type(&self, name: &str) -> Option<&SubjectType> {
        self.subjects.iter().find(|s| s.name == name)
    }

    /// Validate structural invariants.
    ///
    /// Checks:
    /// - Rack and shelf naming: non-empty, no dots, unique.
    /// - Subject-type naming and uniqueness.
    /// - Relation-predicate naming and uniqueness.
    /// - Relation-predicate type-constraint references: every
    ///   non-wildcard name in `source_type` and `target_type` names
    ///   a declared subject type.
    /// - Inverse-predicate consistency: if predicate P declares
    ///   `inverse = Q`, then Q must be declared, Q's source_type
    ///   must equal P's target_type, Q's target_type must equal P's
    ///   source_type, and Q's own `inverse` must be `Some(P)`.
    ///
    /// Each rule fires a distinct `StewardError::Catalogue` variant
    /// with a human-readable message naming the offending item so
    /// a distribution author can locate the problem in their
    /// catalogue file.
    pub fn validate(&self) -> Result<(), StewardError> {
        self.validate_schema_version()?;
        self.validate_racks()?;
        self.validate_subjects()?;
        self.validate_relations()?;
        self.validate_relation_type_references()?;
        self.validate_inverses()?;
        Ok(())
    }

    /// Reject documents whose declared `schema_version` is outside
    /// the steward's supported range.
    ///
    /// The range is integer-valued and contiguous; the steward
    /// supports every version in `[CATALOGUE_SCHEMA_MIN,
    /// CATALOGUE_SCHEMA_MAX]` inclusive. Out-of-range is a hard
    /// startup failure rather than a partial bring-up because the
    /// catalogue is essence: a distribution authored against the
    /// wrong grammar version produces silent feature loss the
    /// operator cannot diagnose.
    fn validate_schema_version(&self) -> Result<(), StewardError> {
        if self.schema_version < CATALOGUE_SCHEMA_MIN
            || self.schema_version > CATALOGUE_SCHEMA_MAX
        {
            return Err(StewardError::Catalogue(format!(
                "catalogue schema_version {} is out of range; this \
                 steward supports [{}, {}]",
                self.schema_version, CATALOGUE_SCHEMA_MIN, CATALOGUE_SCHEMA_MAX,
            )));
        }
        Ok(())
    }

    /// Rack-level validation: naming, charter, shelf-name uniqueness
    /// within each rack.
    fn validate_racks(&self) -> Result<(), StewardError> {
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

    /// Subject-type validation: non-empty, no dots, unique.
    fn validate_subjects(&self) -> Result<(), StewardError> {
        let mut subject_names = std::collections::HashSet::new();
        for subject in &self.subjects {
            if subject.name.is_empty() {
                return Err(StewardError::Catalogue(
                    "subject type with empty name".into(),
                ));
            }
            if subject.name.contains('.') {
                return Err(StewardError::Catalogue(format!(
                    "subject type name must not contain '.': {}",
                    subject.name
                )));
            }
            // The wildcard `"*"` is reserved for predicate
            // source_type / target_type constraints; it would be
            // meaningless as a declared subject type because nothing
            // a plugin announces as type `"*"` could have any
            // concrete meaning. Refuse declaration of `"*"` to keep
            // the reserved token unambiguous.
            if subject.name == "*" {
                return Err(StewardError::Catalogue(
                    "subject type name '*' is reserved (wildcard in \
                     predicate type constraints)"
                        .into(),
                ));
            }
            if !subject_names.insert(&subject.name) {
                return Err(StewardError::Catalogue(format!(
                    "duplicate subject type: {}",
                    subject.name
                )));
            }
        }
        Ok(())
    }

    /// Relation-predicate naming and structural validation (not the
    /// type-reference cross-check; that runs in a later pass over
    /// already-validated subjects).
    fn validate_relations(&self) -> Result<(), StewardError> {
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

    /// Cross-reference: every non-wildcard name appearing in a
    /// predicate's `source_type` or `target_type` must be a declared
    /// subject type.
    ///
    /// The wildcard `"*"` is permitted and skipped. Empty lists are
    /// already rejected by [`Self::validate_relations`].
    fn validate_relation_type_references(&self) -> Result<(), StewardError> {
        for pred in &self.relations {
            for side in [
                ("source_type", &pred.source_type),
                ("target_type", &pred.target_type),
            ] {
                let (label, constraint) = side;
                let names: Vec<&str> = match constraint {
                    TypeConstraint::Single(s) => vec![s.as_str()],
                    TypeConstraint::Multiple(ts) => {
                        ts.iter().map(|s| s.as_str()).collect()
                    }
                };
                for name in names {
                    if name == "*" {
                        continue;
                    }
                    if self.find_subject_type(name).is_none() {
                        return Err(StewardError::Catalogue(format!(
                            "predicate {} {} references undeclared \
                             subject type: {}",
                            pred.predicate, label, name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Inverse-predicate consistency.
    ///
    /// For each predicate P with `inverse = Some(Q)`:
    ///
    /// 1. A predicate named Q must be declared.
    /// 2. Q's `source_type` must equal P's `target_type`.
    /// 3. Q's `target_type` must equal P's `source_type`.
    /// 4. Q's `inverse` must be `Some(P.predicate)`.
    ///
    /// Rule 4 enforces the symmetry `CATALOGUE.md` section 5.3
    /// describes: if both directions are in use they must declare
    /// each other. A predicate may opt out of the inverse index by
    /// omitting `inverse`; rule 4 only fires when the inverse is
    /// declared.
    fn validate_inverses(&self) -> Result<(), StewardError> {
        for pred in &self.relations {
            let Some(inverse_name) = pred.inverse.as_deref() else {
                continue;
            };
            let inverse =
                self.find_predicate(inverse_name).ok_or_else(|| {
                    StewardError::Catalogue(format!(
                        "predicate {} declares inverse {} but that \
                         predicate is not declared",
                        pred.predicate, inverse_name
                    ))
                })?;
            if inverse.source_type != pred.target_type {
                return Err(StewardError::Catalogue(format!(
                    "predicate {} and its declared inverse {} have \
                     mismatched types: {}'s source_type must equal \
                     {}'s target_type",
                    pred.predicate, inverse_name, inverse_name, pred.predicate
                )));
            }
            if inverse.target_type != pred.source_type {
                return Err(StewardError::Catalogue(format!(
                    "predicate {} and its declared inverse {} have \
                     mismatched types: {}'s target_type must equal \
                     {}'s source_type",
                    pred.predicate, inverse_name, inverse_name, pred.predicate
                )));
            }
            match inverse.inverse.as_deref() {
                Some(back) if back == pred.predicate => {}
                Some(other) => {
                    return Err(StewardError::Catalogue(format!(
                        "predicate {} declares inverse {}, but {}'s \
                         own inverse is {} (expected {})",
                        pred.predicate,
                        inverse_name,
                        inverse_name,
                        other,
                        pred.predicate
                    )));
                }
                None => {
                    return Err(StewardError::Catalogue(format!(
                        "predicate {} declares inverse {}, but {} does \
                         not declare its own inverse back to {}",
                        pred.predicate,
                        inverse_name,
                        inverse_name,
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
schema_version = 1

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

    // -----------------------------------------------------------------
    // Rack and shelf parsing + validation
    // -----------------------------------------------------------------

    #[test]
    fn parses_minimal_catalogue() {
        let c = Catalogue::from_toml(MINIMAL).unwrap();
        assert_eq!(c.racks.len(), 1);
        assert_eq!(c.racks[0].name, "example");
        assert_eq!(c.racks[0].shelves.len(), 1);
        assert_eq!(c.racks[0].shelves[0].name, "echo");
        assert_eq!(c.racks[0].shelves[0].shape, 1);
        // Minimal catalogue declares no subject types. This is valid;
        // plugins admitting against it simply cannot announce
        // subjects: undeclared types are refused at the wiring layer.
        assert!(c.subjects.is_empty());
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
schema_version = 1

[[racks]]
name = "a"
family = "domain"
charter = "one"

[[racks]]
name = "a"
family = "domain"
charter = "two"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("duplicate rack name"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_duplicate_shelf_names() {
        let bad = r#"
schema_version = 1

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
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("duplicate shelf name"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_rack_name() {
        let bad = r#"
schema_version = 1

[[racks]]
name = ""
family = "domain"
charter = "x"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        assert!(matches!(err, StewardError::Catalogue(_)));
    }

    #[test]
    fn rejects_rack_name_with_dot() {
        let bad = r#"
schema_version = 1

[[racks]]
name = "a.b"
family = "domain"
charter = "x"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("rack name must not contain '.'"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn schema_version_only_catalogue_is_valid() {
        // A document declaring just `schema_version = 1` parses to an
        // empty catalogue: no racks, no subject types, no relation
        // predicates. This is the minimal admissible catalogue.
        let c = Catalogue::from_toml("schema_version = 1").unwrap();
        assert_eq!(c.schema_version, CATALOGUE_SCHEMA_MIN);
        assert!(c.racks.is_empty());
        assert!(c.subjects.is_empty());
        assert!(c.relations.is_empty());
    }

    #[test]
    fn empty_document_is_rejected_for_missing_schema_version() {
        // Truly empty TOML carries no `schema_version`; the parser
        // refuses it because the field is required (no `serde(default)`)
        // rather than silently defaulting to a version the steward did
        // not promise to support.
        let err = Catalogue::from_toml("").expect_err(
            "empty document must be rejected for missing schema_version",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("schema_version"),
            "error must mention the missing field, got: {msg}"
        );
    }

    #[test]
    fn schema_version_above_max_is_rejected() {
        // A catalogue declaring a schema_version above the steward's
        // CATALOGUE_SCHEMA_MAX is rejected at validation time. This is
        // the path a vN+1 distribution deployed against a vN steward
        // takes; the failure must be explicit, not a silent partial
        // bring-up.
        let toml = format!("schema_version = {}", CATALOGUE_SCHEMA_MAX + 1);
        let err = Catalogue::from_toml(&toml)
            .expect_err("out-of-range schema_version must reject");
        let msg = format!("{err}");
        assert!(
            msg.contains("schema_version") && msg.contains("out of range"),
            "error must name the offending field and the rejection \
             reason, got: {msg}"
        );
    }

    #[test]
    fn schema_version_at_u32_max_is_rejected() {
        // Boundary case: `u32::MAX` is the largest value the field
        // can carry. The validator must reject it with the same
        // out-of-range diagnostic as `MAX + 1` — the boundary itself
        // is well-defined, parses cleanly into the field, and tests
        // that the comparison is "strictly above MAX" rather than
        // "any value parses". A future widening of the supported
        // range could accidentally admit `u32::MAX` if the comparison
        // was loose; this test pins it explicit.
        let toml = format!("schema_version = {}", u32::MAX);
        let err = Catalogue::from_toml(&toml)
            .expect_err("schema_version = u32::MAX must reject");
        let msg = format!("{err}");
        assert!(
            msg.contains("schema_version") && msg.contains("out of range"),
            "error must name the offending field and the rejection \
             reason, got: {msg}"
        );
    }

    #[test]
    fn schema_version_below_min_is_rejected() {
        // schema_version = 0 is below CATALOGUE_SCHEMA_MIN = 1. Same
        // rejection path as above-max; the test pins the symmetric case
        // so a future widening of the supported range cannot silently
        // accept it.
        let err = Catalogue::from_toml("schema_version = 0")
            .expect_err("schema_version = 0 must reject");
        let msg = format!("{err}");
        assert!(
            msg.contains("schema_version") && msg.contains("out of range"),
            "error must name the offending field and the rejection \
             reason, got: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // Subject-type declarations and validation
    // -----------------------------------------------------------------

    #[test]
    fn parses_subject_type_declarations() {
        let toml = r#"
schema_version = 1

[[subjects]]
name = "track"
description = "A playable audio item."

[[subjects]]
name = "album"
description = "A collection of tracks with shared metadata."
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        assert_eq!(c.subjects.len(), 2);
        assert_eq!(c.subjects[0].name, "track");
        assert_eq!(c.subjects[1].name, "album");
    }

    #[test]
    fn find_subject_type_returns_declared() {
        let toml = r#"
schema_version = 1

[[subjects]]
name = "track"
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        assert!(c.find_subject_type("track").is_some());
        assert!(c.find_subject_type("album").is_none());
    }

    #[test]
    fn rejects_duplicate_subject_types() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "track"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("duplicate subject type"),
                    "unexpected message: {msg}"
                );
                assert!(msg.contains("track"));
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_subject_type_name() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = ""
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("subject type with empty name"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_subject_type_name_with_dot() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "foo.bar"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("subject type name must not contain '.'"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wildcard_subject_type_name() {
        // `"*"` is reserved for predicate type constraints and may
        // not be a declared subject type.
        let bad = r#"
schema_version = 1

[[subjects]]
name = "*"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(msg.contains("reserved"), "unexpected message: {msg}");
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Relation predicate parsing and existing structural validation
    // -----------------------------------------------------------------

    #[test]
    fn parses_relation_predicate_with_single_types() {
        // Declares both subject types that the predicates reference.
        // Without the [[subjects]] section this catalogue would fail
        // the type-reference check even though the predicate
        // structure is otherwise valid.
        let toml = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

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
schema_version = 1

[[subjects]]
name = "folder"

[[subjects]]
name = "archive"

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
                assert_eq!(
                    ts,
                    &vec!["folder".to_string(), "archive".to_string()]
                );
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
schema_version = 1

[[subjects]]
name = "t"

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
        // validate_relations runs before validate_relation_type_references
        // so the duplicate is reported even though `a`/`b`/`c`/`d`
        // are undeclared. The test pins that ordering: reorganising
        // validate() to check type references before dedup would
        // surface a different error and we would want to know.
        let toml = r#"
schema_version = 1

[[subjects]]
name = "a"

[[subjects]]
name = "b"

[[subjects]]
name = "c"

[[subjects]]
name = "d"

[[relation]]
predicate = "p"
source_type = "a"
target_type = "b"

[[relation]]
predicate = "p"
source_type = "c"
target_type = "d"
"#;
        let err = Catalogue::from_toml(toml).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("duplicate relation predicate"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_predicate_with_dot() {
        let toml = r#"
schema_version = 1

[[subjects]]
name = "x"

[[subjects]]
name = "y"

[[relation]]
predicate = "a.b"
source_type = "x"
target_type = "y"
"#;
        let err = Catalogue::from_toml(toml).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("predicate name must not contain '.'"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_predicate_with_empty_name() {
        let toml = r#"
schema_version = 1

[[subjects]]
name = "x"

[[subjects]]
name = "y"

[[relation]]
predicate = ""
source_type = "x"
target_type = "y"
"#;
        let err = Catalogue::from_toml(toml).unwrap_err();
        assert!(matches!(err, StewardError::Catalogue(_)));
    }

    #[test]
    fn rejects_predicate_with_empty_multiple_types() {
        let toml = r#"
schema_version = 1

[[subjects]]
name = "t"

[[relation]]
predicate = "p"
source_type = []
target_type = "t"
"#;
        let err = Catalogue::from_toml(toml).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("empty source_type list"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Predicate type-reference cross-check
    // -----------------------------------------------------------------

    #[test]
    fn rejects_predicate_with_undeclared_source_type() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("source_type"),
                    "unexpected message: {msg}"
                );
                assert!(msg.contains("track"));
                assert!(msg.contains("album_of"));
                assert!(
                    msg.contains("undeclared"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_predicate_with_undeclared_target_type() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "track"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("target_type"),
                    "unexpected message: {msg}"
                );
                assert!(msg.contains("album"));
                assert!(msg.contains("album_of"));
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_predicate_with_undeclared_type_in_multiple() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "folder"

[[relation]]
predicate = "contained_in"
source_type = "*"
target_type = ["folder", "archive"]
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("target_type"),
                    "unexpected message: {msg}"
                );
                assert!(msg.contains("archive"));
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn accepts_predicate_with_wildcard_type_reference() {
        // `"*"` is the explicit wildcard for predicate type
        // constraints and must pass even when no subject types are
        // declared. This is the escape hatch for truly generic
        // predicates (e.g. `contained_in`) that span every subject
        // kind.
        let toml = r#"
schema_version = 1

[[relation]]
predicate = "contained_in"
source_type = "*"
target_type = "*"
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        assert_eq!(c.relations.len(), 1);
    }

    // -----------------------------------------------------------------
    // Inverse predicate consistency
    // -----------------------------------------------------------------

    #[test]
    fn accepts_consistent_inverse_predicates() {
        let toml = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "album"
target_type = "track"
inverse = "album_of"
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        assert_eq!(c.relations.len(), 2);
    }

    #[test]
    fn rejects_inverse_pointing_at_missing_predicate() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
inverse = "tracks_of"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("inverse tracks_of"),
                    "unexpected message: {msg}"
                );
                assert!(
                    msg.contains("not declared"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_inverse_with_mismatched_source_type() {
        // `tracks_of` should have source_type = "album" (the target
        // type of `album_of`). Here it is declared as "track",
        // which breaks the symmetry.
        let bad = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "track"
target_type = "track"
inverse = "album_of"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("mismatched types"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_inverse_with_mismatched_target_type() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "album"
target_type = "album"
inverse = "album_of"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("mismatched types"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_inverse_that_does_not_point_back() {
        let bad = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "album"
target_type = "track"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("does not declare"),
                    "unexpected message: {msg}"
                );
                assert!(msg.contains("tracks_of"));
                assert!(msg.contains("album_of"));
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_inverse_pointing_at_wrong_predicate() {
        // tracks_of says its inverse is `other_predicate`, not
        // album_of. Catch the asymmetry.
        let bad = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "album"
target_type = "track"
inverse = "other_predicate"

[[relation]]
predicate = "other_predicate"
source_type = "track"
target_type = "album"
inverse = "tracks_of"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Catalogue(msg) => {
                assert!(
                    msg.contains("own inverse is")
                        || msg.contains("does not declare"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Catalogue error, got {other:?}"),
        }
    }

    #[test]
    fn predicate_without_inverse_is_valid() {
        // A predicate may opt out of the inverse index entirely.
        // Inverse validation only fires when `inverse` is set.
        let toml = r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
"#;
        let c = Catalogue::from_toml(toml).unwrap();
        assert!(c.find_predicate("album_of").unwrap().inverse.is_none());
    }

    /// A catalogue file missing the required `schema_version`
    /// field MUST cause boot to fail with an error that names the
    /// missing field. The path is the operator-facing surface, so
    /// the error MUST be loud enough to point at the cause without
    /// further investigation.
    #[test]
    fn missing_schema_version_refuses_with_clear_error() {
        // No `schema_version` line at all. `serde` reports this as
        // a missing-field error on the parse step (the field is
        // non-Optional in the catalogue type).
        let bad = r#"
[[racks]]
name = "r"
family = "domain"
charter = "missing schema_version"
"#;
        let err = Catalogue::from_toml(bad).unwrap_err();
        match err {
            StewardError::Toml { source, .. } => {
                let msg = source.to_string();
                assert!(
                    msg.contains("schema_version"),
                    "missing-field error must name `schema_version`, got: \
                     {msg}"
                );
            }
            other => panic!("expected Toml parse error, got {other:?}"),
        }
    }

    /// Same scenario routed through `Catalogue::load` from a real
    /// file on disk. Pins the boot-time refusal: a malformed
    /// catalogue at the configured path MUST stop the steward
    /// before it serves any client.
    #[test]
    fn load_from_path_refuses_malformed_catalogue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalogue.toml");
        std::fs::write(
            &path,
            r#"
[[racks]]
name = "r"
family = "domain"
charter = "missing schema_version"
"#,
        )
        .unwrap();
        let err = Catalogue::load(&path).unwrap_err();
        match err {
            StewardError::Toml { context, source } => {
                assert!(
                    context.contains(&path.display().to_string()),
                    "context must name the offending file path, got: \
                     {context}"
                );
                assert!(
                    source.to_string().contains("schema_version"),
                    "underlying error must name the missing field, got: \
                     {source}"
                );
            }
            other => panic!("expected Toml error, got {other:?}"),
        }
    }
}
