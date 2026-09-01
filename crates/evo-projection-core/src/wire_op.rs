// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Wire-operation identity and metadata.
//!
//! Every operation the wire protocol exposes has a stable
//! identifier ([`WireOpId`]) and projection-friendly metadata
//! ([`WireOp`]). The framework's annotated wire-protocol types
//! emit a [`WireOp`] per variant; every projection and every SDK
//! generator consumes the resulting set.

use crate::audit::AuditTiming;
use crate::capability::CapabilityRequirement;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// A stable identifier for a wire operation.
///
/// Identifiers are snake-case ASCII, non-empty, and stable across
/// protocol versions. Examples: `"request"`,
/// `"describe_capabilities"`, `"project_subject"`,
/// `"subscribe_happenings"`. New operations land as new IDs; an
/// existing operation's ID never changes.
///
/// Construction is fallible via [`WireOpId::new`]; the wrapped
/// string is validated against the snake-case regex
/// `^[a-z][a-z0-9_]*$` so a typo in an annotation surfaces at
/// build time rather than as a runtime mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WireOpId(String);

impl WireOpId {
    /// Construct a `WireOpId` from a string, validating the
    /// snake-case shape.
    ///
    /// Returns [`WireOpIdError::Empty`] on empty input,
    /// [`WireOpIdError::InvalidShape`] on input that does not
    /// match `^[a-z][a-z0-9_]*$`.
    pub fn new(id: impl Into<String>) -> Result<Self, WireOpIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(WireOpIdError::Empty);
        }
        if !is_valid_wire_op_id(&id) {
            return Err(WireOpIdError::InvalidShape(id));
        }
        Ok(Self(id))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WireOpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for WireOpId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Validation errors produced by [`WireOpId::new`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireOpIdError {
    /// The supplied identifier was empty.
    #[error("wire op id must not be empty")]
    Empty,

    /// The supplied identifier did not match
    /// `^[a-z][a-z0-9_]*$`.
    #[error(
        "wire op id '{0}' must be snake_case ASCII (regex: \
         ^[a-z][a-z0-9_]*$)"
    )]
    InvalidShape(String),
}

/// Full metadata for one wire operation.
///
/// Each annotated wire-protocol variant emits a `WireOp` describing
/// its identity, capability gate, audit emission timing, and
/// projection-friendly hints. The resulting set is the schema every
/// projection and SDK generator reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireOp {
    /// Stable identifier for this operation.
    pub id: WireOpId,

    /// Capability scope a caller must hold to dispatch this op.
    pub capability: CapabilityRequirement,

    /// When this op emits to the audit ledger.
    pub audit: AuditTiming,

    /// One-line human-readable summary. Surfaces in generated SDK
    /// docstrings, GraphQL field descriptions, OpenAPI summaries.
    /// Plain prose, no markdown.
    pub summary: String,
}

impl WireOp {
    /// Construct a `WireOp` with the supplied identity + scopes.
    pub fn new(
        id: WireOpId,
        capability: CapabilityRequirement,
        audit: AuditTiming,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id,
            capability,
            audit,
            summary: summary.into(),
        }
    }
}

fn is_valid_wire_op_id(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_op_id_accepts_snake_case() {
        let id = WireOpId::new("describe_capabilities").unwrap();
        assert_eq!(id.as_str(), "describe_capabilities");
    }

    #[test]
    fn wire_op_id_accepts_single_word() {
        let id = WireOpId::new("request").unwrap();
        assert_eq!(id.as_str(), "request");
    }

    #[test]
    fn wire_op_id_accepts_digits_after_first_letter() {
        let id = WireOpId::new("project_subject_v2").unwrap();
        assert_eq!(id.as_str(), "project_subject_v2");
    }

    #[test]
    fn wire_op_id_refuses_empty() {
        assert_eq!(WireOpId::new(""), Err(WireOpIdError::Empty));
    }

    #[test]
    fn wire_op_id_refuses_leading_digit() {
        assert_eq!(
            WireOpId::new("2describe"),
            Err(WireOpIdError::InvalidShape("2describe".to_string()))
        );
    }

    #[test]
    fn wire_op_id_refuses_leading_underscore() {
        assert_eq!(
            WireOpId::new("_describe"),
            Err(WireOpIdError::InvalidShape("_describe".to_string()))
        );
    }

    #[test]
    fn wire_op_id_refuses_uppercase() {
        assert_eq!(
            WireOpId::new("Describe"),
            Err(WireOpIdError::InvalidShape("Describe".to_string()))
        );
    }

    #[test]
    fn wire_op_id_refuses_hyphen() {
        assert_eq!(
            WireOpId::new("describe-capabilities"),
            Err(WireOpIdError::InvalidShape(
                "describe-capabilities".to_string()
            ))
        );
    }

    #[test]
    fn wire_op_id_refuses_whitespace() {
        assert_eq!(
            WireOpId::new("describe capabilities"),
            Err(WireOpIdError::InvalidShape(
                "describe capabilities".to_string()
            ))
        );
    }

    #[test]
    fn wire_op_id_round_trips_through_serde() {
        let id = WireOpId::new("describe_capabilities").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"describe_capabilities\"");
        let back: WireOpId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn wire_op_carries_full_metadata() {
        let op = WireOp::new(
            WireOpId::new("describe_capabilities").unwrap(),
            CapabilityRequirement::None,
            AuditTiming::None,
            "Return supported wire ops and named features.",
        );
        assert_eq!(op.id.as_str(), "describe_capabilities");
        assert_eq!(op.capability, CapabilityRequirement::None);
        assert_eq!(op.audit, AuditTiming::None);
        assert!(op.summary.contains("supported wire ops"));
    }

    #[test]
    fn wire_op_round_trips_through_serde() {
        let op = WireOp::new(
            WireOpId::new("set_update_channel").unwrap(),
            CapabilityRequirement::step_up("plugins_admin"),
            AuditTiming::Always,
            "Set the update channel for core or plugins.",
        );
        let json = serde_json::to_string(&op).unwrap();
        let back: WireOp = serde_json::from_str(&json).unwrap();
        assert_eq!(back, op);
    }
}
