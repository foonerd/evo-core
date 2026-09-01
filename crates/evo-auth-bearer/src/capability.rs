// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Typed capability surface for bearer tokens.
//!
//! Mirrors the wire-protocol schema's
//! [`CapabilityRequirement`] taxonomy on the operator side:
//! a token carries one or more [`Capability`] values
//! describing what the holder may invoke. Capability
//! satisfaction is rank-ordered: `Write(s)` satisfies
//! `Read(s)`; `StepUp(s)` satisfies `Write(s)` + `Read(s)`
//! + `StepUp(s)`.

use evo_projection_core::CapabilityRequirement;
use serde::{Deserialize, Serialize};

/// One typed capability the token holder may exercise.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Capability {
    /// Read-scope on the named capability.
    Read {
        /// The capability scope identifier.
        scope: String,
    },
    /// Write-scope on the named capability. Satisfies
    /// `Read(scope)` AND `Write(scope)` requirements.
    Write {
        /// The capability scope identifier.
        scope: String,
    },
    /// Step-up-scope on the named capability. Satisfies
    /// `Read(scope)` + `Write(scope)` + `StepUp(scope)`
    /// requirements.
    StepUp {
        /// The capability scope identifier.
        scope: String,
    },
}

impl Capability {
    /// Construct a `Read` capability on the named scope.
    pub fn read(scope: impl Into<String>) -> Self {
        Self::Read {
            scope: scope.into(),
        }
    }

    /// Construct a `Write` capability on the named scope.
    pub fn write(scope: impl Into<String>) -> Self {
        Self::Write {
            scope: scope.into(),
        }
    }

    /// Construct a `StepUp` capability on the named scope.
    pub fn step_up(scope: impl Into<String>) -> Self {
        Self::StepUp {
            scope: scope.into(),
        }
    }

    /// Borrow the scope identifier this capability carries.
    pub fn scope(&self) -> &str {
        match self {
            Self::Read { scope }
            | Self::Write { scope }
            | Self::StepUp { scope } => scope.as_str(),
        }
    }

    /// Whether this capability satisfies the supplied wire
    /// op requirement.
    ///
    /// Satisfaction rules:
    ///
    /// - Any capability satisfies
    ///   [`CapabilityRequirement::None`] (anonymous ops).
    /// - `Read(scope)` satisfies `Read(scope)` only.
    /// - `Write(scope)` satisfies `Read(scope)` AND
    ///   `Write(scope)`.
    /// - `StepUp(scope)` satisfies `Read(scope)` +
    ///   `Write(scope)` + `StepUp(scope)`.
    ///
    /// Cross-scope satisfaction is refused: a `Write(audio)`
    /// capability does NOT satisfy `Read(plugins)`.
    pub fn satisfies(&self, req: &CapabilityRequirement) -> bool {
        match req {
            CapabilityRequirement::None => true,
            CapabilityRequirement::Read { scope } => self.scope() == scope,
            CapabilityRequirement::Write { scope } => {
                self.scope() == scope
                    && matches!(self, Self::Write { .. } | Self::StepUp { .. })
            }
            CapabilityRequirement::StepUp { scope } => {
                self.scope() == scope && matches!(self, Self::StepUp { .. })
            }
        }
    }
}

/// A set of capabilities carried by one bearer token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(Vec<Capability>);

impl CapabilitySet {
    /// Construct from an iterable of capabilities.
    pub fn new(caps: impl IntoIterator<Item = Capability>) -> Self {
        Self(caps.into_iter().collect())
    }

    /// Borrow the underlying capability list.
    pub fn capabilities(&self) -> &[Capability] {
        &self.0
    }

    /// Total number of capabilities in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether any capability in this set satisfies the
    /// supplied requirement.
    pub fn satisfies(&self, req: &CapabilityRequirement) -> bool {
        // Anonymous-OK requirements are satisfied by any set,
        // including the empty set (defense-in-depth: even an
        // empty token may dispatch `describe_capabilities`).
        if matches!(req, CapabilityRequirement::None) {
            return true;
        }
        self.0.iter().any(|c| c.satisfies(req))
    }
}

impl From<Vec<Capability>> for CapabilitySet {
    fn from(v: Vec<Capability>) -> Self {
        Self(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_satisfies_only_matching_read() {
        let cap = Capability::read("plugins");
        assert!(cap.satisfies(&CapabilityRequirement::read("plugins")));
        assert!(!cap.satisfies(&CapabilityRequirement::write("plugins")));
        assert!(!cap.satisfies(&CapabilityRequirement::step_up("plugins")));
        assert!(!cap.satisfies(&CapabilityRequirement::read("audio")));
    }

    #[test]
    fn write_satisfies_read_and_write_same_scope() {
        let cap = Capability::write("plugins");
        assert!(cap.satisfies(&CapabilityRequirement::read("plugins")));
        assert!(cap.satisfies(&CapabilityRequirement::write("plugins")));
        assert!(!cap.satisfies(&CapabilityRequirement::step_up("plugins")));
        assert!(!cap.satisfies(&CapabilityRequirement::write("audio")));
    }

    #[test]
    fn step_up_satisfies_every_rank_same_scope() {
        let cap = Capability::step_up("plugins_admin");
        assert!(cap.satisfies(&CapabilityRequirement::read("plugins_admin")));
        assert!(cap.satisfies(&CapabilityRequirement::write("plugins_admin")));
        assert!(cap.satisfies(&CapabilityRequirement::step_up("plugins_admin")));
        assert!(!cap.satisfies(&CapabilityRequirement::step_up("audio_admin")));
    }

    #[test]
    fn any_capability_satisfies_anonymous() {
        for c in [
            Capability::read("a"),
            Capability::write("b"),
            Capability::step_up("c"),
        ] {
            assert!(c.satisfies(&CapabilityRequirement::None));
        }
    }

    #[test]
    fn empty_set_satisfies_anonymous_only() {
        let s = CapabilitySet::default();
        assert!(s.satisfies(&CapabilityRequirement::None));
        assert!(!s.satisfies(&CapabilityRequirement::read("plugins")));
    }

    #[test]
    fn set_satisfies_when_any_member_does() {
        let s = CapabilitySet::new(vec![
            Capability::read("audio"),
            Capability::write("plugins"),
        ]);
        assert!(s.satisfies(&CapabilityRequirement::read("audio")));
        assert!(s.satisfies(&CapabilityRequirement::write("plugins")));
        assert!(s.satisfies(&CapabilityRequirement::read("plugins")));
        assert!(!s.satisfies(&CapabilityRequirement::step_up("plugins")));
    }

    #[test]
    fn capability_round_trips_through_serde() {
        for c in [
            Capability::read("a"),
            Capability::write("b"),
            Capability::step_up("c"),
        ] {
            let json = serde_json::to_string(&c).unwrap();
            let back: Capability = serde_json::from_str(&json).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn capability_set_round_trips_through_serde() {
        let s = CapabilitySet::new(vec![
            Capability::read("plugins"),
            Capability::step_up("plugins_admin"),
        ]);
        let json = serde_json::to_string(&s).unwrap();
        let back: CapabilitySet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn capability_set_accessors_report_size() {
        let s = CapabilitySet::new(vec![
            Capability::read("a"),
            Capability::write("b"),
        ]);
        assert_eq!(s.len(), 2);
        assert!(!s.is_empty());
        assert_eq!(s.capabilities().len(), 2);

        let empty = CapabilitySet::default();
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn cross_scope_satisfaction_is_refused() {
        // Step-up on `audio_admin` does NOT satisfy a
        // step-up requirement on `plugins_admin` even
        // though both are step-up rank.
        let cap = Capability::step_up("audio_admin");
        assert!(
            !cap.satisfies(&CapabilityRequirement::step_up("plugins_admin"))
        );
    }
}
