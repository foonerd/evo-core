// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Plugin filter language.
//!
//! A typed, composable predicate over admitted (and known)
//! plugins. Drives the bulk-operation wire ops:
//!
//! - `disable_where(filter)`
//! - `enable_where(filter)`
//! - `purge_state_where(filter)`
//! - `list_where(filter)` (preview surface; the operator-facing
//!   "the following N plugins match; proceed?" pattern)
//!
//! The filter is *evaluated*, not *executed*: a [`PluginContext`]
//! is built per plugin from the router + persistence + manifest +
//! tags, and [`PluginFilter::matches`] returns a boolean. The
//! caller (the wire op handler) iterates the candidate set,
//! collects matches, and performs the per-plugin lifecycle
//! transition.
//!
//! Filter shape mirrors the design ADR's enum (`ById`,
//! `ByPublisher`, `ByCapability`, `ByTag`, `ByVersionLessThan`,
//! `ByCurrentState`, `And` / `Or` / `Not`, `All`). The
//! `ByLastUsedBefore` variant in the design lives in a future
//! iteration alongside the activity-tracking primitive (no
//! activity substrate today). Inclusion matches what the
//! framework can evaluate honestly given the substrate that
//! exists; it is not a scope reduction — the missing variant
//! gates on a separate primitive.
//!
//! The current publisher derivation rule: take the canonical
//! plugin name (reverse-DNS, e.g. `com.example.audio`) and use
//! the first two dot-separated components as the publisher id
//! (`com.example`). This matches operator intuition and works
//! for unsigned and signed bundles uniformly. A future iteration
//! can replace the derivation with the signing-key publisher
//! mapping when that surface lands.

use std::collections::HashSet;

use semver::Version;
use serde::{Deserialize, Serialize};

/// Currently observable lifecycle state for filter matching.
/// Maps to the four states the framework can report today;
/// `Suspended` lives in the power-management primitive (future
/// release) and will land alongside that work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginCurrentState {
    /// Admitted on the router and the operator has not disabled
    /// it.
    Enabled,
    /// Recorded in `installed_plugins` with `enabled = false`;
    /// not currently admitted on the router.
    Disabled,
    /// No `installed_plugins` row at all — the plugin was never
    /// admitted on this device, or has been uninstalled.
    NotAdmitted,
}

impl PluginCurrentState {
    /// Stable lowercase wire string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::NotAdmitted => "not_admitted",
        }
    }

    /// Parse a wire string into the enum.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            "not_admitted" => Some(Self::NotAdmitted),
            _ => None,
        }
    }
}

/// Per-plugin context the filter evaluates against. Built by the
/// wire op handler from the router + persistence + manifest +
/// tags substrate; passed by reference so allocations are
/// amortised across a fleet-scale match.
#[derive(Debug, Clone)]
pub struct PluginContext {
    /// Canonical plugin name (manifest's `plugin.name`).
    pub canonical_name: String,
    /// Derived publisher id (first two dot-separated components
    /// of `canonical_name`). Empty string when the canonical
    /// name has fewer than two segments.
    pub publisher_id: String,
    /// Plugin version from the manifest. `None` when the
    /// manifest is unavailable (legacy plugins admitted via
    /// non-manifest paths).
    pub version: Option<Version>,
    /// Declared capability tokens (presence-style flags from
    /// the manifest's `[capabilities]` table — `"respondent"`,
    /// `"warden"`, `"factory"`, `"source"`, `"admin"`,
    /// `"fast_path"`, `"appointments"`, `"watches"`,
    /// `"metadata"`, `"scheduler"`, `"notifications"`,
    /// `"credentials"`, `"streams"`, `"queue"`, `"plans"`).
    pub capabilities: HashSet<String>,
    /// Operator-applied tags (from the `plugin_tags` substrate).
    pub tags: HashSet<String>,
    /// Currently observable lifecycle state.
    pub current_state: PluginCurrentState,
}

impl PluginContext {
    /// Construct a context with the publisher id derived from
    /// the canonical plugin name. Convenience builder used by
    /// the wire op handler.
    pub fn new(
        canonical_name: impl Into<String>,
        version: Option<Version>,
        capabilities: impl IntoIterator<Item = String>,
        tags: impl IntoIterator<Item = String>,
        current_state: PluginCurrentState,
    ) -> Self {
        let canonical_name = canonical_name.into();
        let publisher_id = derive_publisher_id(&canonical_name);
        Self {
            canonical_name,
            publisher_id,
            version,
            capabilities: capabilities.into_iter().collect(),
            tags: tags.into_iter().collect(),
            current_state,
        }
    }
}

/// Derive a publisher id from a reverse-DNS canonical name.
/// `com.example.audio` → `com.example`; `org.evoframework.x.y`
/// → `org.evoframework`. Names with fewer than two dot-separated
/// components return the empty string (no publisher inference
/// possible).
pub fn derive_publisher_id(canonical_name: &str) -> String {
    let mut parts = canonical_name.splitn(3, '.');
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => {
            format!("{a}.{b}")
        }
        _ => String::new(),
    }
}

/// Composable predicate language over [`PluginContext`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginFilter {
    /// Match every plugin.
    All,
    /// Match plugins whose canonical name is in the supplied
    /// set.
    ById {
        /// Canonical names to match. Empty list matches nothing.
        ids: Vec<String>,
    },
    /// Match plugins whose derived publisher id equals the
    /// supplied value.
    ByPublisher {
        /// Publisher id (e.g. `com.example`).
        publisher_id: String,
    },
    /// Match plugins that declare the supplied capability token
    /// in their manifest's `[capabilities]` table.
    ByCapability {
        /// Capability token (`"source"`, `"warden"`, `"admin"`,
        /// `"appointments"`, …).
        capability: String,
    },
    /// Match plugins that carry the supplied tag.
    ByTag {
        /// Tag string.
        tag: String,
    },
    /// Match plugins whose version is strictly less than the
    /// supplied value. Plugins without a manifest-recorded
    /// version (`None` in the context) do not match.
    ByVersionLessThan {
        /// Plugins WITH the supplied canonical name AND a
        /// version less than `version` match. Filters that
        /// want to apply across multiple plugin names use
        /// `Or` of `ByVersionLessThan` instances.
        plugin_name: String,
        /// Strict upper bound.
        version: Version,
    },
    /// Match plugins whose currently observable lifecycle
    /// state equals the supplied value.
    ByCurrentState {
        /// Lifecycle state.
        state: PluginCurrentState,
    },
    /// Logical conjunction. An empty `terms` list matches every
    /// plugin (the identity element of `And`).
    And {
        /// Term filters; ALL must match.
        terms: Vec<PluginFilter>,
    },
    /// Logical disjunction. An empty `terms` list matches no
    /// plugin (the identity element of `Or`).
    Or {
        /// Term filters; ANY may match.
        terms: Vec<PluginFilter>,
    },
    /// Logical negation.
    Not {
        /// Filter to negate.
        term: Box<PluginFilter>,
    },
}

impl PluginFilter {
    /// Evaluate the filter against the supplied context.
    pub fn matches(&self, ctx: &PluginContext) -> bool {
        match self {
            Self::All => true,
            Self::ById { ids } => ids.contains(&ctx.canonical_name),
            Self::ByPublisher { publisher_id } => {
                ctx.publisher_id == *publisher_id
            }
            Self::ByCapability { capability } => {
                ctx.capabilities.contains(capability)
            }
            Self::ByTag { tag } => ctx.tags.contains(tag),
            Self::ByVersionLessThan {
                plugin_name,
                version,
            } => {
                ctx.canonical_name == *plugin_name
                    && match &ctx.version {
                        Some(v) => v < version,
                        None => false,
                    }
            }
            Self::ByCurrentState { state } => ctx.current_state == *state,
            Self::And { terms } => terms.iter().all(|t| t.matches(ctx)),
            Self::Or { terms } => terms.iter().any(|t| t.matches(ctx)),
            Self::Not { term } => !term.matches(ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(name: &str, state: PluginCurrentState) -> PluginContext {
        PluginContext::new(name, Some(Version::new(1, 0, 0)), [], [], state)
    }

    #[test]
    fn derive_publisher_id_two_segment() {
        assert_eq!(derive_publisher_id("com.example.audio"), "com.example");
    }

    #[test]
    fn derive_publisher_id_three_segment_takes_first_two() {
        assert_eq!(
            derive_publisher_id("org.evoframework.x.y"),
            "org.evoframework"
        );
    }

    #[test]
    fn derive_publisher_id_single_segment_is_empty() {
        assert_eq!(derive_publisher_id("standalone"), "");
        assert_eq!(derive_publisher_id(""), "");
    }

    #[test]
    fn current_state_parse_roundtrip() {
        for s in [
            PluginCurrentState::Enabled,
            PluginCurrentState::Disabled,
            PluginCurrentState::NotAdmitted,
        ] {
            assert_eq!(PluginCurrentState::parse(s.as_str()), Some(s));
        }
        assert!(PluginCurrentState::parse("running").is_none());
    }

    #[test]
    fn all_matches_every_plugin() {
        let f = PluginFilter::All;
        assert!(f.matches(&ctx("com.x.y", PluginCurrentState::Enabled)));
        assert!(f.matches(&ctx("com.a.b", PluginCurrentState::NotAdmitted)));
    }

    #[test]
    fn by_id_matches_listed_names() {
        let f = PluginFilter::ById {
            ids: vec!["com.x.y".into(), "com.a.b".into()],
        };
        assert!(f.matches(&ctx("com.x.y", PluginCurrentState::Enabled)));
        assert!(f.matches(&ctx("com.a.b", PluginCurrentState::Disabled)));
        assert!(!f.matches(&ctx("com.other.q", PluginCurrentState::Enabled)));
    }

    #[test]
    fn by_id_empty_matches_nothing() {
        let f = PluginFilter::ById { ids: vec![] };
        assert!(!f.matches(&ctx("com.x.y", PluginCurrentState::Enabled)));
    }

    #[test]
    fn by_publisher_matches_derived_id() {
        let f = PluginFilter::ByPublisher {
            publisher_id: "com.example".into(),
        };
        assert!(
            f.matches(&ctx("com.example.audio", PluginCurrentState::Enabled,))
        );
        assert!(f.matches(&ctx(
            "com.example.metadata",
            PluginCurrentState::Disabled,
        )));
        assert!(!f
            .matches(
                &ctx("com.different.audio", PluginCurrentState::Enabled,)
            ));
    }

    #[test]
    fn by_capability_matches_declared_capability() {
        let c = PluginContext::new(
            "com.x.y",
            None,
            ["source".into(), "respondent".into()],
            [],
            PluginCurrentState::Enabled,
        );
        let f = PluginFilter::ByCapability {
            capability: "source".into(),
        };
        assert!(f.matches(&c));
        let f = PluginFilter::ByCapability {
            capability: "warden".into(),
        };
        assert!(!f.matches(&c));
    }

    #[test]
    fn by_tag_matches_tagged_plugin() {
        let c = PluginContext::new(
            "com.x.y",
            None,
            [],
            ["audiophile".into(), "experimental".into()],
            PluginCurrentState::Enabled,
        );
        let f = PluginFilter::ByTag {
            tag: "audiophile".into(),
        };
        assert!(f.matches(&c));
        let f = PluginFilter::ByTag {
            tag: "casual".into(),
        };
        assert!(!f.matches(&c));
    }

    #[test]
    fn by_version_less_than_matches_correct_name_and_lower_version() {
        let c = PluginContext::new(
            "com.x.y",
            Some(Version::new(1, 2, 3)),
            [],
            [],
            PluginCurrentState::Enabled,
        );
        let f = PluginFilter::ByVersionLessThan {
            plugin_name: "com.x.y".into(),
            version: Version::new(2, 0, 0),
        };
        assert!(f.matches(&c));
        // Different name → no match even though version
        // qualifies.
        let f = PluginFilter::ByVersionLessThan {
            plugin_name: "com.other".into(),
            version: Version::new(2, 0, 0),
        };
        assert!(!f.matches(&c));
        // Same name, version equal to bound → no match (strict
        // less-than).
        let f = PluginFilter::ByVersionLessThan {
            plugin_name: "com.x.y".into(),
            version: Version::new(1, 2, 3),
        };
        assert!(!f.matches(&c));
        // Same name, version greater than bound → no match.
        let f = PluginFilter::ByVersionLessThan {
            plugin_name: "com.x.y".into(),
            version: Version::new(1, 0, 0),
        };
        assert!(!f.matches(&c));
    }

    #[test]
    fn by_version_less_than_no_recorded_version_does_not_match() {
        let c = PluginContext::new(
            "com.x.y",
            None,
            [],
            [],
            PluginCurrentState::Enabled,
        );
        let f = PluginFilter::ByVersionLessThan {
            plugin_name: "com.x.y".into(),
            version: Version::new(2, 0, 0),
        };
        assert!(!f.matches(&c));
    }

    #[test]
    fn by_current_state_matches_observable_state() {
        let f = PluginFilter::ByCurrentState {
            state: PluginCurrentState::Disabled,
        };
        assert!(f.matches(&ctx("com.x.y", PluginCurrentState::Disabled)));
        assert!(!f.matches(&ctx("com.x.y", PluginCurrentState::Enabled)));
    }

    #[test]
    fn and_empty_is_identity_match_all() {
        let f = PluginFilter::And { terms: vec![] };
        assert!(f.matches(&ctx("com.x.y", PluginCurrentState::Enabled)));
    }

    #[test]
    fn or_empty_is_identity_match_none() {
        let f = PluginFilter::Or { terms: vec![] };
        assert!(!f.matches(&ctx("com.x.y", PluginCurrentState::Enabled)));
    }

    #[test]
    fn and_requires_all_terms() {
        let f = PluginFilter::And {
            terms: vec![
                PluginFilter::ByPublisher {
                    publisher_id: "com.example".into(),
                },
                PluginFilter::ByCurrentState {
                    state: PluginCurrentState::Disabled,
                },
            ],
        };
        let c1 = ctx("com.example.x", PluginCurrentState::Disabled);
        assert!(f.matches(&c1));
        let c2 = ctx("com.example.x", PluginCurrentState::Enabled);
        assert!(!f.matches(&c2));
        let c3 = ctx("com.other.x", PluginCurrentState::Disabled);
        assert!(!f.matches(&c3));
    }

    #[test]
    fn or_matches_any_term() {
        let f = PluginFilter::Or {
            terms: vec![
                PluginFilter::ByPublisher {
                    publisher_id: "com.example".into(),
                },
                PluginFilter::ByCurrentState {
                    state: PluginCurrentState::NotAdmitted,
                },
            ],
        };
        assert!(f.matches(&ctx("com.example.x", PluginCurrentState::Enabled)));
        assert!(f.matches(&ctx("com.other.x", PluginCurrentState::NotAdmitted)));
        assert!(!f.matches(&ctx("com.other.x", PluginCurrentState::Enabled)));
    }

    #[test]
    fn not_negates() {
        let f = PluginFilter::Not {
            term: Box::new(PluginFilter::ByCurrentState {
                state: PluginCurrentState::Enabled,
            }),
        };
        assert!(f.matches(&ctx("com.x.y", PluginCurrentState::Disabled)));
        assert!(!f.matches(&ctx("com.x.y", PluginCurrentState::Enabled)));
    }

    #[test]
    fn nested_composition_evaluates_correctly() {
        // (publisher = com.example AND NOT enabled) OR by-tag legacy
        let f = PluginFilter::Or {
            terms: vec![
                PluginFilter::And {
                    terms: vec![
                        PluginFilter::ByPublisher {
                            publisher_id: "com.example".into(),
                        },
                        PluginFilter::Not {
                            term: Box::new(PluginFilter::ByCurrentState {
                                state: PluginCurrentState::Enabled,
                            }),
                        },
                    ],
                },
                PluginFilter::ByTag {
                    tag: "legacy".into(),
                },
            ],
        };

        // com.example with disabled → first arm matches.
        let c1 = PluginContext::new(
            "com.example.audio",
            None,
            [],
            [],
            PluginCurrentState::Disabled,
        );
        assert!(f.matches(&c1));

        // com.example enabled → first arm fails; tag legacy
        // not present → no match.
        let c2 = PluginContext::new(
            "com.example.audio",
            None,
            [],
            [],
            PluginCurrentState::Enabled,
        );
        assert!(!f.matches(&c2));

        // com.other enabled with tag legacy → second arm
        // matches.
        let c3 = PluginContext::new(
            "com.other.x",
            None,
            [],
            ["legacy".into()],
            PluginCurrentState::Enabled,
        );
        assert!(f.matches(&c3));
    }

    #[test]
    fn serde_round_trip_preserves_filter_shape() {
        let f = PluginFilter::Or {
            terms: vec![
                PluginFilter::ByPublisher {
                    publisher_id: "com.example".into(),
                },
                PluginFilter::Not {
                    term: Box::new(PluginFilter::ByCurrentState {
                        state: PluginCurrentState::Disabled,
                    }),
                },
            ],
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: PluginFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }
}
