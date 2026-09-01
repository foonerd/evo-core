// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Capability scoping for wire operations.
//!
//! Every wire operation declares the capability scope its caller
//! must hold to dispatch it. Concrete projections apply this
//! declaration uniformly across every transport (REST, WebSocket,
//! gRPC, GraphQL, HTTP/3): a [`CapabilityRequirement::StepUp`] op
//! refuses on every surface without active step-up auth, including
//! any future surface that does not yet exist when the op is
//! authored.
//!
//! ## Token model
//!
//! The framework's capability-scoped bearer-token model carries
//! the set of capabilities the holder may exercise on the token
//! itself, not as bearer-of-everything. The token issuer (the
//! framework's auth layer at admission time) embeds the
//! capability set; every projection verifies the inbound
//! token's capabilities against the op's
//! [`CapabilityRequirement`] before dispatch. The verifier path
//! is uniform: the projection layer never decides scope
//! independently per surface; surface-specific decisions would
//! defeat the schema-first invariant.

use serde::{Deserialize, Serialize};

/// The capability scope a caller must hold to dispatch one wire
/// operation.
///
/// The variants form a strict lattice: anonymous-OK is the
/// floor; `Read` requires a token bearing the named read scope;
/// `Write` requires a token bearing the named write scope;
/// `StepUp` requires a token bearing the named write scope AND
/// an active step-up auth session within the step-up TTL.
///
/// The named scope strings (`"plugins_admin"`, `"network_admin"`,
/// `"playback"`, etc.) follow the existing framework convention
/// for capability identifiers. Projections do not enumerate the
/// scope set independently — every scope used in
/// [`CapabilityRequirement::Read`] /
/// [`CapabilityRequirement::Write`] /
/// [`CapabilityRequirement::StepUp`] must exist in the
/// framework's capability registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityRequirement {
    /// Anonymous-OK. The operation accepts dispatches without a
    /// capability check. Reserved for read-only discovery ops
    /// like `describe_capabilities` whose response shape itself
    /// drives the client's downstream gating decisions.
    None,

    /// Caller must hold a token bearing read scope on the named
    /// capability. The variant's payload is the capability name,
    /// e.g. `"playback"`, `"network"`, `"plugins"`.
    Read {
        /// The capability scope the caller must hold for read.
        scope: String,
    },

    /// Caller must hold a token bearing write scope on the
    /// named capability. The variant's payload is the capability
    /// name; the same set of names as [`Self::Read`] is reused
    /// (the read/write split is on the token, not on the
    /// capability registry).
    Write {
        /// The capability scope the caller must hold for write.
        scope: String,
    },

    /// Caller must hold a token bearing write scope on the
    /// named capability AND have completed step-up auth within
    /// the session's step-up TTL. Reserved for privileged
    /// operations: plugin admission, capability grant
    /// revocation, SSH state changes, update apply, steward
    /// restart, etc.
    StepUp {
        /// The capability scope the caller must hold for
        /// step-up write.
        scope: String,
    },
}

impl CapabilityRequirement {
    /// Construct a `Read` requirement against the named scope.
    pub fn read(scope: impl Into<String>) -> Self {
        Self::Read {
            scope: scope.into(),
        }
    }

    /// Construct a `Write` requirement against the named scope.
    pub fn write(scope: impl Into<String>) -> Self {
        Self::Write {
            scope: scope.into(),
        }
    }

    /// Construct a `StepUp` requirement against the named
    /// scope.
    pub fn step_up(scope: impl Into<String>) -> Self {
        Self::StepUp {
            scope: scope.into(),
        }
    }

    /// Whether this requirement gates the operation behind any
    /// capability check. `None` returns `false`; every other
    /// variant returns `true`.
    pub fn is_gated(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this requirement demands an active step-up auth
    /// session. Only [`Self::StepUp`] returns `true`.
    pub fn requires_step_up(&self) -> bool {
        matches!(self, Self::StepUp { .. })
    }

    /// Borrow the scope string if this variant carries one.
    /// `None` returns `None`.
    pub fn scope(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Read { scope }
            | Self::Write { scope }
            | Self::StepUp { scope } => Some(scope.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_not_gated_and_has_no_scope() {
        let r = CapabilityRequirement::None;
        assert!(!r.is_gated());
        assert!(!r.requires_step_up());
        assert_eq!(r.scope(), None);
    }

    #[test]
    fn read_is_gated_but_not_step_up() {
        let r = CapabilityRequirement::read("playback");
        assert!(r.is_gated());
        assert!(!r.requires_step_up());
        assert_eq!(r.scope(), Some("playback"));
    }

    #[test]
    fn write_is_gated_but_not_step_up() {
        let r = CapabilityRequirement::write("network");
        assert!(r.is_gated());
        assert!(!r.requires_step_up());
        assert_eq!(r.scope(), Some("network"));
    }

    #[test]
    fn step_up_is_gated_and_demands_step_up() {
        let r = CapabilityRequirement::step_up("plugins_admin");
        assert!(r.is_gated());
        assert!(r.requires_step_up());
        assert_eq!(r.scope(), Some("plugins_admin"));
    }

    #[test]
    fn requirements_round_trip_through_serde() {
        for r in [
            CapabilityRequirement::None,
            CapabilityRequirement::read("playback"),
            CapabilityRequirement::write("network"),
            CapabilityRequirement::step_up("plugins_admin"),
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: CapabilityRequirement =
                serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn none_serialises_to_tagged_object() {
        let r = CapabilityRequirement::None;
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"kind":"none"}"#);
    }

    #[test]
    fn step_up_serialises_with_scope() {
        let r = CapabilityRequirement::step_up("plugins_admin");
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"kind":"step_up","scope":"plugins_admin"}"#);
    }
}
