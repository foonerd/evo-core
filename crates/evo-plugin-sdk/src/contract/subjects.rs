//! Subject identity types.
//!
//! Carries the data types plugins use to address and announce subjects,
//! per `docs/engineering/SUBJECTS.md`. The [`SubjectAnnouncer`] trait
//! itself lives in [`context`](super::context) alongside the other
//! callback traits.
//!
//! ## The shape of subject identity
//!
//! A subject has a canonical identifier (opaque to plugins, assigned by
//! the steward) and zero or more external addressings (the identifiers
//! plugins use natively). Plugins see only external addressings; the
//! steward translates at the boundary.
//!
//! - [`ExternalAddressing`]: a `(scheme, value)` pair a plugin uses to
//!   refer to a subject in its native ID space.
//! - [`CanonicalSubjectId`]: the steward's opaque identifier. Plugins
//!   do not construct these; they are visible only to consumers of
//!   projections.
//! - [`SubjectAnnouncement`]: the payload a plugin sends when announcing
//!   a subject: type + addressings + optional claims.
//! - [`SubjectClaim`]: an equivalence or distinctness claim between two
//!   addressings.
//! - [`ClaimConfidence`]: how certain the claimant is.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// An external addressing: a `(scheme, value)` pair a plugin uses to refer
/// to a subject in its native ID space.
///
/// Schemes are globally unique kebab-case strings. A plugin owns the
/// schemes it declares in its manifest; only the owner may introduce new
/// values in that scheme.
///
/// Values are opaque bytes encoded as strings; the steward never parses
/// them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ExternalAddressing {
    /// Scheme identifier, kebab-case, lowercase.
    pub scheme: String,
    /// Opaque value within that scheme.
    pub value: String,
}

impl ExternalAddressing {
    /// Construct an addressing from scheme and value.
    pub fn new(scheme: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            value: value.into(),
        }
    }
}

impl std::fmt::Display for ExternalAddressing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.scheme, self.value)
    }
}

/// A canonical subject identifier assigned by the steward.
///
/// Opaque to plugins. Plugins do not construct these. The type exists in
/// the SDK so that callback-return types are nameable and so that
/// future SDK features needing to reference canonical IDs (e.g.
/// relation assertions) have a concrete handle.
///
/// Stored internally as a string for wire-format compatibility; the
/// steward's native representation is typically a UUID v4 but the SDK
/// does not commit to that format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CanonicalSubjectId(pub String);

impl CanonicalSubjectId {
    /// Construct from a raw string. Plugins should not call this; the
    /// steward does.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CanonicalSubjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How certain a claimant is about a subject identity claim.
///
/// Per `SUBJECTS.md` section 8.2. Used by the steward's reconciliation
/// logic to break ties when claims disagree.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ClaimConfidence {
    /// Claimant is certain. External service gave the mapping directly
    /// or identity is provable.
    Asserted,
    /// Claimant computed the claim from observable attributes.
    Inferred,
    /// Claimant's best guess. Used for fuzzy matching where the claim
    /// may be wrong.
    Tentative,
}

/// A claim about subject identity between two external addressings.
///
/// Per `SUBJECTS.md` sections 8.1 and 8.3. Claims are observations, not
/// live assertions; they persist in the registry independent of the
/// claimant's lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubjectClaim {
    /// The two addressings refer to the same subject.
    Equivalent {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
        /// How certain the claimant is.
        confidence: ClaimConfidence,
        /// Free-form explanation.
        reason: Option<String>,
    },
    /// The two addressings refer to different subjects.
    Distinct {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
        /// Free-form explanation.
        reason: Option<String>,
    },
}

/// A subject announcement from a plugin.
///
/// Per `SUBJECTS.md` section 7.2. Carries:
///
/// - The subject type (must be one the catalogue declared).
/// - The external addressings the announcing plugin knows for this
///   subject. At least one MUST be in a scheme the plugin owns; zero or
///   more may be in readonly schemes the plugin references.
/// - Optional claims about equivalence or distinctness involving the
///   announced addressings or addressings the plugin has observed
///   elsewhere.
/// - A timestamp; the steward may override with its own reception time.
///
/// All addressings in a single announcement are treated as equivalent:
/// by announcing them together, the plugin asserts they refer to one
/// subject. Explicit `SubjectClaim` entries add metadata (confidence,
/// reason) to specific pairs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubjectAnnouncement {
    /// Subject type, must be declared in the catalogue.
    pub subject_type: String,
    /// External addressings identifying this subject.
    pub addressings: Vec<ExternalAddressing>,
    /// Optional equivalence or distinctness claims carried with this
    /// announcement.
    #[serde(default)]
    pub claims: Vec<SubjectClaim>,
    /// When the announcement was generated on the plugin side.
    pub announced_at: SystemTime,
}

impl SubjectAnnouncement {
    /// Construct an announcement with current time and no claims.
    pub fn new(
        subject_type: impl Into<String>,
        addressings: Vec<ExternalAddressing>,
    ) -> Self {
        Self {
            subject_type: subject_type.into(),
            addressings,
            claims: Vec::new(),
            announced_at: SystemTime::now(),
        }
    }

    /// Add a claim to this announcement.
    pub fn with_claim(mut self, claim: SubjectClaim) -> Self {
        self.claims.push(claim);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_addressing_display() {
        let a = ExternalAddressing::new("spotify", "track:abc");
        assert_eq!(format!("{a}"), "spotify:track:abc");
    }

    #[test]
    fn external_addressing_equality_is_scheme_and_value() {
        let a = ExternalAddressing::new("spotify", "track:abc");
        let b = ExternalAddressing::new("spotify", "track:abc");
        let c = ExternalAddressing::new("mbid", "track:abc");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn canonical_id_displays_as_inner() {
        let id = CanonicalSubjectId::new("a1b2-c3d4");
        assert_eq!(format!("{id}"), "a1b2-c3d4");
        assert_eq!(id.as_str(), "a1b2-c3d4");
    }

    #[test]
    fn confidence_serialises_lowercase() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            c: ClaimConfidence,
        }
        let t = toml::to_string(&Wrap {
            c: ClaimConfidence::Asserted,
        })
        .unwrap();
        assert!(t.contains(r#"c = "asserted""#));
        let parsed: Wrap = toml::from_str(r#"c = "inferred""#).unwrap();
        assert_eq!(parsed.c, ClaimConfidence::Inferred);
    }

    #[test]
    fn subject_announcement_new_captures_time() {
        let before = SystemTime::now();
        let a = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/music/foo.flac")],
        );
        let after = SystemTime::now();
        assert_eq!(a.subject_type, "track");
        assert_eq!(a.addressings.len(), 1);
        assert!(a.claims.is_empty());
        assert!(a.announced_at >= before);
        assert!(a.announced_at <= after);
    }

    #[test]
    fn subject_announcement_with_claim() {
        let a = SubjectAnnouncement::new(
            "track",
            vec![
                ExternalAddressing::new("spotify", "track:X"),
                ExternalAddressing::new("mbid", "abc-def"),
            ],
        )
        .with_claim(SubjectClaim::Equivalent {
            a: ExternalAddressing::new("spotify", "track:X"),
            b: ExternalAddressing::new("mbid", "abc-def"),
            confidence: ClaimConfidence::Asserted,
            reason: Some("Spotify API returned the MBID".into()),
        });
        assert_eq!(a.claims.len(), 1);
    }

    #[test]
    fn subject_claim_equivalent_serialises() {
        let c = SubjectClaim::Equivalent {
            a: ExternalAddressing::new("s1", "v1"),
            b: ExternalAddressing::new("s2", "v2"),
            confidence: ClaimConfidence::Asserted,
            reason: None,
        };
        let t = toml::to_string(&c).unwrap();
        assert!(t.contains(r#"kind = "equivalent""#));
        assert!(t.contains(r#"confidence = "asserted""#));
    }

    #[test]
    fn subject_claim_distinct_serialises() {
        let c = SubjectClaim::Distinct {
            a: ExternalAddressing::new("s1", "v1"),
            b: ExternalAddressing::new("s2", "v2"),
            reason: Some("different ISRCs".into()),
        };
        let t = toml::to_string(&c).unwrap();
        assert!(t.contains(r#"kind = "distinct""#));
    }
}
