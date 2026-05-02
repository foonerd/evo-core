//! Relation assertion types.
//!
//! Carries the data types plugins use to assert and retract relations
//! between subjects, per `docs/engineering/RELATIONS.md`. The
//! [`RelationAnnouncer`] trait itself lives in
//! [`context`](super::context) alongside the other callback traits.
//!
//! ## The shape of a relation
//!
//! A relation is a directed quad:
//!
//! ```text
//! (source_subject, predicate, target_subject, provenance_set)
//! ```
//!
//! where `source_subject` and `target_subject` are subject identities
//! and `predicate` is a name declared in the catalogue. The provenance
//! set is maintained by the steward as plugins assert and retract
//! their claims; plugins never see it directly.
//!
//! ## Plugin-side vs steward-side addressing
//!
//! Plugins refer to subjects by [`ExternalAddressing`] as always. The
//! steward resolves addressings to canonical IDs internally and stores
//! relations in canonical-ID space. If either the source or target
//! addressing does not resolve to an existing subject at assertion
//! time, the announcement is rejected with a
//! [`ReportError::Invalid`](super::context::ReportError::Invalid).
//! Plugins should announce subjects before asserting relations about
//! them.

use crate::contract::subjects::ExternalAddressing;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// A relation assertion from a plugin.
///
/// Per `RELATIONS.md` section 4. The plugin identifies the source and
/// target by external addressing, names the predicate declared in the
/// catalogue, and optionally explains the reason. The steward records
/// the plugin as a claimant on the resulting relation; multiple plugins
/// may claim the same relation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationAssertion {
    /// Source subject addressing.
    pub source: ExternalAddressing,
    /// Predicate name, must be declared in the catalogue.
    pub predicate: String,
    /// Target subject addressing.
    pub target: ExternalAddressing,
    /// Free-form explanation of why this relation is being asserted.
    pub reason: Option<String>,
    /// When the assertion was generated on the plugin side.
    pub asserted_at: SystemTime,
}

impl RelationAssertion {
    /// Construct an assertion with current time and no reason.
    pub fn new(
        source: ExternalAddressing,
        predicate: impl Into<String>,
        target: ExternalAddressing,
    ) -> Self {
        Self {
            source,
            predicate: predicate.into(),
            target,
            reason: None,
            asserted_at: SystemTime::now(),
        }
    }

    /// Attach a reason to this assertion.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// A relation retraction from a plugin.
///
/// Per `RELATIONS.md` section 4.3. Plugins may retract only their own
/// claims; if the relation has no remaining claimants after retraction,
/// the steward removes it. Cross-plugin retractions are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationRetraction {
    /// Source subject addressing.
    pub source: ExternalAddressing,
    /// Predicate name.
    pub predicate: String,
    /// Target subject addressing.
    pub target: ExternalAddressing,
    /// Free-form explanation for audit.
    pub reason: Option<String>,
}

impl RelationRetraction {
    /// Construct a retraction with no reason.
    pub fn new(
        source: ExternalAddressing,
        predicate: impl Into<String>,
        target: ExternalAddressing,
    ) -> Self {
        Self {
            source,
            predicate: predicate.into(),
            target,
            reason: None,
        }
    }

    /// Attach a reason to this retraction.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(scheme: &str, value: &str) -> ExternalAddressing {
        ExternalAddressing::new(scheme, value)
    }

    #[test]
    fn assertion_new_captures_time() {
        let before = SystemTime::now();
        let a = RelationAssertion::new(
            addr("mpd-path", "/a.flac"),
            "album_of",
            addr("mbid", "album-x"),
        );
        let after = SystemTime::now();
        assert_eq!(a.predicate, "album_of");
        assert!(a.reason.is_none());
        assert!(a.asserted_at >= before);
        assert!(a.asserted_at <= after);
    }

    #[test]
    fn assertion_with_reason() {
        let a = RelationAssertion::new(addr("s", "v1"), "p", addr("s", "v2"))
            .with_reason("scraped from metadata");
        assert_eq!(a.reason.as_deref(), Some("scraped from metadata"));
    }

    #[test]
    fn retraction_new_no_reason() {
        let r = RelationRetraction::new(addr("s", "v1"), "p", addr("s", "v2"));
        assert!(r.reason.is_none());
    }

    #[test]
    fn retraction_with_reason() {
        let r = RelationRetraction::new(addr("s", "v1"), "p", addr("s", "v2"))
            .with_reason("file deleted");
        assert_eq!(r.reason.as_deref(), Some("file deleted"));
    }

    #[test]
    fn assertion_serialises() {
        let a = RelationAssertion::new(
            addr("mpd-path", "/a.flac"),
            "album_of",
            addr("mbid", "album-x"),
        )
        .with_reason("test");
        let t = toml::to_string(&a).unwrap();
        assert!(t.contains(r#"predicate = "album_of""#));
        assert!(t.contains(r#"reason = "test""#));
    }
}
