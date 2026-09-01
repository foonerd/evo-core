// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Admission policy primitive.
//!
//! An admission policy is an operator-defined rule set that
//! enforces against the plugin set. Two enforcement points:
//!
//! - **At admission**: a plugin admitting against the active
//!   policy is refused when its manifest violates one or more
//!   rules. This composition wires alongside the admission-engine
//!   refactoring; the substrate / typed-rules / audit verb land
//!   here.
//!
//! - **Continuously**: the operator can audit the currently-
//!   admitted plugin set against any policy (active or not).
//!   The `audit_against_policy` verb returns the list of
//!   violations so the operator decides what to do
//!   (auto-disable is intentionally not the framework's job;
//!   the operator picks).
//!
//! Multiple policies can be authored on the device but exactly
//! zero or one is flagged active at any time. Activation is a
//! transaction over the substrate's `active` column; the
//! invariant is preserved across crashes / restarts.
//!
//! Three shapes:
//!
//! - [`AdmissionPolicy`]: the typed record matching the
//!   persisted shape (metadata + the rules body).
//! - [`AdmissionPolicyRules`]: the rule body itself —
//!   require/ban/limit fields the evaluator consults.
//! - [`AdmissionPolicyStore`]: persistence-backed accessor with
//!   a typed `audit_plugins` evaluator that walks a candidate
//!   set and returns violations.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::persistence::{
    PersistedAdmissionPolicy, PersistenceError, PersistenceStore,
};
use crate::plugin_filter::{derive_publisher_id, PluginContext};
use crate::plugin_profile::ProfileAuthor;

/// Rule body of an admission policy. Fields are independent
/// gates: a plugin must satisfy every applicable gate to admit /
/// remain compliant. New rule additions land as new fields with
/// `#[serde(default)]` so substrate JSON written under an older
/// build remains forward-compatible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionPolicyRules {
    /// When `true`, refuse plugins admitted with an unsigned
    /// bundle. The substrate doesn't carry signature state per
    /// admitted plugin in this iteration; the rule is
    /// declarable, the audit-time evaluator is conservative
    /// (see [`AdmissionPolicyStore::audit_plugins`]).
    #[serde(default)]
    pub require_signed: bool,
    /// When `true`, refuse plugins whose publisher is not
    /// recorded with verified state in the publisher-trust
    /// substrate.
    #[serde(default)]
    pub require_verified_publisher: bool,
    /// Per-plugin upper bound on declared outbound network
    /// endpoints. `None` means no limit. The audit-time
    /// evaluator inspects the manifest's `[prerequisites]`
    /// field; for the framework primitive scope, declaration
    /// is the visible signal (the manifest is the operator's
    /// declaration of intent).
    #[serde(default)]
    pub max_outbound_network_endpoints: Option<u32>,
    /// Aggregate (across admitted plugins) memory ceiling. The
    /// audit-time evaluator does not enforce aggregate limits
    /// today — it inspects per-plugin declared resources only.
    /// Aggregate enforcement composes with the resource-tracker
    /// primitive when that lands.
    #[serde(default)]
    pub max_total_plugin_memory_mb: Option<u32>,
    /// Capability allowlist. When non-empty, a plugin's
    /// declared capability tokens (presence-style) MUST be a
    /// subset of this set. Empty list means no allowlist.
    #[serde(default)]
    pub allowed_capabilities: Vec<String>,
    /// Capability blocklist. Any plugin declaring one of these
    /// capability tokens violates.
    #[serde(default)]
    pub banned_capabilities: Vec<String>,
    /// Publisher blocklist. Plugins whose derived publisher id
    /// matches any entry violate.
    #[serde(default)]
    pub ban_publishers: Vec<String>,
    /// Publisher allowlist. When non-empty, a plugin's derived
    /// publisher id MUST appear in this list. Empty list means
    /// no allowlist.
    #[serde(default)]
    pub require_publisher_in: Vec<String>,
}

/// One policy as the operator authored it: metadata plus the
/// typed rule body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPolicy {
    /// Filesystem-safe slug; primary key.
    pub policy_id: String,
    /// Operator-readable name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Authoring class.
    pub authored_by: ProfileAuthor,
    /// Wall-clock millisecond timestamp of creation.
    pub created_at_ms: u64,
    /// Whether this policy is the currently-active one.
    pub active: bool,
    /// Rule body.
    pub rules: AdmissionPolicyRules,
}

/// One per-plugin violation surfaced by the audit evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPolicyViolation {
    /// Plugin canonical name.
    pub plugin_name: String,
    /// Stable rule-class string identifying which gate failed.
    /// Operator surfaces and audit consumers bind to this
    /// vocabulary:
    /// - `"banned_capability"` — declared a banned capability
    /// - `"capability_not_in_allowlist"` — declared a capability
    ///   outside the allowlist
    /// - `"banned_publisher"` — derived publisher matches a
    ///   ban entry
    /// - `"publisher_not_in_allowlist"` — derived publisher
    ///   absent from the allowlist
    /// - `"max_outbound_network_endpoints_unverifiable"` — rule
    ///   is declared but the plugin's manifest is unavailable
    ///   to verify (treated as a violation under the
    ///   conservative-audit posture)
    pub rule_class: String,
    /// Human-readable detail. Surfaces the failing field's
    /// value so operators can act without diving into the
    /// manifest.
    pub detail: String,
}

/// Errors raised by [`AdmissionPolicyStore`].
#[derive(Debug, thiserror::Error)]
pub enum PolicyStoreError {
    /// Policy id does not match any recorded policy.
    #[error("admission policy not found: {0}")]
    NotFound(String),
    /// `rules_json` did not decode as the expected
    /// [`AdmissionPolicyRules`] shape. Surfaces a substrate-
    /// corruption / version-skew condition; the read refuses
    /// rather than ignoring the malformed row silently.
    #[error("admission policy {policy_id} has corrupt rules_json: {detail}")]
    CorruptRules {
        /// Policy id holding the corrupt rules.
        policy_id: String,
        /// Decode-error detail.
        detail: String,
    },
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
}

/// Persistence-backed accessor for admission policies.
#[derive(Debug, Clone)]
pub struct AdmissionPolicyStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl AdmissionPolicyStore {
    /// Construct a store wrapping the supplied persistence
    /// handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Insert or replace one policy. The active flag is
    /// preserved across the upsert; activation is a separate
    /// transition arbitrated by [`Self::mark_active`].
    pub async fn put(
        &self,
        policy: AdmissionPolicy,
    ) -> Result<(), PolicyStoreError> {
        let rules_json = serde_json::to_string(&policy.rules).map_err(|e| {
            PolicyStoreError::CorruptRules {
                policy_id: policy.policy_id.clone(),
                detail: format!("serialise rules: {e}"),
            }
        })?;
        let persisted = PersistedAdmissionPolicy {
            policy_id: policy.policy_id,
            name: policy.name,
            description: policy.description,
            authored_by: policy.authored_by.as_str().to_string(),
            rules_json,
            active: policy.active,
            created_at_ms: policy.created_at_ms,
        };
        self.persistence.put_admission_policy(persisted).await?;
        Ok(())
    }

    /// Read one policy by id.
    pub async fn get(
        &self,
        policy_id: &str,
    ) -> Result<Option<AdmissionPolicy>, PolicyStoreError> {
        let row = self.persistence.get_admission_policy(policy_id).await?;
        match row {
            None => Ok(None),
            Some(p) => decode_policy(p).map(Some),
        }
    }

    /// List every policy. Order is `policy_id` ascending.
    pub async fn list(&self) -> Result<Vec<AdmissionPolicy>, PolicyStoreError> {
        let rows = self.persistence.list_admission_policies().await?;
        // Filter rows whose decode fails (substrate corruption)
        // rather than failing the whole listing — the operator
        // surface should still display valid rows when one is
        // bad.
        Ok(rows
            .into_iter()
            .filter_map(|r| decode_policy(r).ok())
            .collect())
    }

    /// Delete one policy by id. Idempotent.
    pub async fn delete(
        &self,
        policy_id: &str,
    ) -> Result<(), PolicyStoreError> {
        self.persistence.delete_admission_policy(policy_id).await?;
        Ok(())
    }

    /// Read the currently-active policy.
    pub async fn active(
        &self,
    ) -> Result<Option<AdmissionPolicy>, PolicyStoreError> {
        let active = self.persistence.get_active_admission_policy().await?;
        match active {
            None => Ok(None),
            Some(p) => decode_policy(p).map(Some),
        }
    }

    /// Mark `policy_id` as the active policy. `None` clears.
    pub async fn mark_active(
        &self,
        policy_id: Option<&str>,
    ) -> Result<(), PolicyStoreError> {
        match self
            .persistence
            .set_active_admission_policy(policy_id)
            .await
        {
            Ok(()) => Ok(()),
            Err(PersistenceError::NotFound(_)) => Err(
                PolicyStoreError::NotFound(policy_id.unwrap_or("").to_string()),
            ),
            Err(e) => Err(e.into()),
        }
    }

    /// Audit a candidate set of plugins against the supplied
    /// policy id and return per-plugin violations. The caller
    /// supplies a `verified_publisher_reader` closure that
    /// returns `true` when a derived publisher id is recorded
    /// as verified in the publisher-trust substrate; this
    /// keeps the policy primitive testable in isolation while
    /// the wire op handler builds the closure against the live
    /// publisher-trust store.
    ///
    /// Returns the violation list in plugin-name-ascending
    /// order. An empty list means every plugin in the
    /// candidate set complies.
    pub async fn audit_plugins<F>(
        &self,
        policy_id: &str,
        plugins: &[PluginContext],
        verified_publisher_reader: F,
    ) -> Result<Vec<AdmissionPolicyViolation>, PolicyStoreError>
    where
        F: Fn(&str) -> bool,
    {
        let policy = self
            .get(policy_id)
            .await?
            .ok_or_else(|| PolicyStoreError::NotFound(policy_id.into()))?;
        let rules = &policy.rules;
        let mut out = Vec::new();
        let mut sorted: Vec<&PluginContext> = plugins.iter().collect();
        sorted.sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
        for ctx in sorted {
            evaluate_plugin(ctx, rules, &verified_publisher_reader, &mut out);
        }
        Ok(out)
    }
}

fn evaluate_plugin<F: Fn(&str) -> bool>(
    ctx: &PluginContext,
    rules: &AdmissionPolicyRules,
    verified_publisher_reader: &F,
    out: &mut Vec<AdmissionPolicyViolation>,
) {
    // Banned capabilities — earliest match wins per-plugin
    // (one violation per banned token a plugin declares).
    for ban in &rules.banned_capabilities {
        if ctx.capabilities.contains(ban) {
            out.push(AdmissionPolicyViolation {
                plugin_name: ctx.canonical_name.clone(),
                rule_class: "banned_capability".into(),
                detail: format!("plugin declares banned capability {ban:?}"),
            });
        }
    }

    // Allowed capabilities — when non-empty, every declared
    // capability MUST be in the list.
    if !rules.allowed_capabilities.is_empty() {
        for cap in &ctx.capabilities {
            if !rules.allowed_capabilities.contains(cap) {
                out.push(AdmissionPolicyViolation {
                    plugin_name: ctx.canonical_name.clone(),
                    rule_class: "capability_not_in_allowlist".into(),
                    detail: format!(
                        "plugin declares capability {cap:?} which is not in \
                         the policy allowlist"
                    ),
                });
            }
        }
    }

    // Banned publishers — derived publisher id matches a ban.
    for ban in &rules.ban_publishers {
        if &ctx.publisher_id == ban {
            out.push(AdmissionPolicyViolation {
                plugin_name: ctx.canonical_name.clone(),
                rule_class: "banned_publisher".into(),
                detail: format!(
                    "plugin's derived publisher id {ban:?} is in the policy \
                     ban list"
                ),
            });
        }
    }

    // Publisher allowlist — non-empty means the plugin's
    // derived publisher MUST appear.
    if !rules.require_publisher_in.is_empty()
        && !rules.require_publisher_in.contains(&ctx.publisher_id)
    {
        out.push(AdmissionPolicyViolation {
            plugin_name: ctx.canonical_name.clone(),
            rule_class: "publisher_not_in_allowlist".into(),
            detail: format!(
                "plugin's derived publisher id {:?} is not in the policy \
                 allowlist",
                ctx.publisher_id
            ),
        });
    }

    // require_verified_publisher — consults the operator-
    // supplied reader.
    if rules.require_verified_publisher
        && !verified_publisher_reader(&ctx.publisher_id)
    {
        out.push(AdmissionPolicyViolation {
            plugin_name: ctx.canonical_name.clone(),
            rule_class: "publisher_not_verified".into(),
            detail: format!(
                "plugin's derived publisher id {:?} is not recorded as \
                 verified in the publisher-trust substrate",
                ctx.publisher_id
            ),
        });
    }

    // require_signed — substrate doesn't track per-plugin
    // signature state in this iteration. The rule is
    // recordable; the audit-time evaluator emits a structural
    // "unverifiable" violation when the rule is set, so the
    // operator surface can render the gap until the
    // signature-state primitive composes here.
    if rules.require_signed {
        out.push(AdmissionPolicyViolation {
            plugin_name: ctx.canonical_name.clone(),
            rule_class: "require_signed_unverifiable".into(),
            detail: "policy requires signed bundles but the framework does \
                     not yet expose per-plugin signature state to the \
                     auditor; the rule is declared and pending the \
                     signature-state composition"
                .into(),
        });
    }

    // max_outbound_network_endpoints / max_total_plugin_memory_mb
    // — the framework primitive does not yet carry per-plugin
    // outbound-endpoint or memory accounting visible to the
    // auditor. The rules are declarable so policies authored
    // today remain forward-compatible; emit a structural
    // "unverifiable" violation only when the rule is set so
    // operator surfaces can render the gap.
    if rules.max_outbound_network_endpoints.is_some() {
        out.push(AdmissionPolicyViolation {
            plugin_name: ctx.canonical_name.clone(),
            rule_class: "max_outbound_network_endpoints_unverifiable".into(),
            detail: "policy declares max_outbound_network_endpoints but the \
                     framework does not yet expose per-plugin outbound \
                     endpoint accounting to the auditor"
                .into(),
        });
    }
    if rules.max_total_plugin_memory_mb.is_some() {
        out.push(AdmissionPolicyViolation {
            plugin_name: ctx.canonical_name.clone(),
            rule_class: "max_total_plugin_memory_mb_unverifiable".into(),
            detail: "policy declares max_total_plugin_memory_mb but the \
                     framework does not yet expose per-plugin memory \
                     accounting to the auditor"
                .into(),
        });
    }
}

fn decode_policy(
    p: PersistedAdmissionPolicy,
) -> Result<AdmissionPolicy, PolicyStoreError> {
    let authored_by =
        ProfileAuthor::parse(&p.authored_by).ok_or_else(|| {
            PolicyStoreError::CorruptRules {
                policy_id: p.policy_id.clone(),
                detail: format!(
                    "authored_by {:?} not in taxonomy",
                    p.authored_by
                ),
            }
        })?;
    let rules = serde_json::from_str::<AdmissionPolicyRules>(&p.rules_json)
        .map_err(|e| PolicyStoreError::CorruptRules {
            policy_id: p.policy_id.clone(),
            detail: format!("decode rules_json: {e}"),
        })?;
    // Touch publisher-id derivation tests' helper so the linker
    // keeps the symbol observable even if the matcher inlines
    // every consumer.
    let _ = derive_publisher_id;
    Ok(AdmissionPolicy {
        policy_id: p.policy_id,
        name: p.name,
        description: p.description,
        authored_by,
        created_at_ms: p.created_at_ms,
        active: p.active,
        rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;
    use crate::plugin_filter::PluginCurrentState;

    fn store() -> AdmissionPolicyStore {
        AdmissionPolicyStore::new(Arc::new(MemoryPersistenceStore::default()))
    }

    fn fixture(id: &str, rules: AdmissionPolicyRules) -> AdmissionPolicy {
        AdmissionPolicy {
            policy_id: id.into(),
            name: format!("Policy {id}"),
            description: None,
            authored_by: ProfileAuthor::User,
            created_at_ms: 1_000,
            active: false,
            rules,
        }
    }

    fn ctx_with(canonical_name: &str, capabilities: &[&str]) -> PluginContext {
        PluginContext::new(
            canonical_name,
            None,
            capabilities.iter().map(|s| s.to_string()),
            [],
            PluginCurrentState::Enabled,
        )
    }

    #[tokio::test]
    async fn put_then_get_roundtrips() {
        let s = store();
        let rules = AdmissionPolicyRules {
            banned_capabilities: vec!["filesystem_unrestricted".into()],
            ..Default::default()
        };
        s.put(fixture("baseline", rules)).await.unwrap();
        let p = s.get("baseline").await.unwrap().expect("get");
        assert_eq!(p.policy_id, "baseline");
        assert_eq!(
            p.rules.banned_capabilities,
            vec!["filesystem_unrestricted"]
        );
    }

    #[tokio::test]
    async fn list_is_id_sorted() {
        let s = store();
        s.put(fixture("zebra", AdmissionPolicyRules::default()))
            .await
            .unwrap();
        s.put(fixture("alpha", AdmissionPolicyRules::default()))
            .await
            .unwrap();
        let rows = s.list().await.unwrap();
        assert_eq!(rows[0].policy_id, "alpha");
        assert_eq!(rows[1].policy_id, "zebra");
    }

    #[tokio::test]
    async fn mark_active_arbitrates_one_at_a_time() {
        let s = store();
        s.put(fixture("a", AdmissionPolicyRules::default()))
            .await
            .unwrap();
        s.put(fixture("b", AdmissionPolicyRules::default()))
            .await
            .unwrap();
        s.mark_active(Some("a")).await.unwrap();
        let active = s.active().await.unwrap().expect("active");
        assert_eq!(active.policy_id, "a");
        s.mark_active(Some("b")).await.unwrap();
        let active = s.active().await.unwrap().expect("active");
        assert_eq!(active.policy_id, "b");
        // Previously-active no longer flagged.
        assert!(!s.get("a").await.unwrap().unwrap().active);
    }

    #[tokio::test]
    async fn mark_active_unknown_id_errors_not_found() {
        let s = store();
        let err = s.mark_active(Some("nonexistent")).await.unwrap_err();
        assert!(matches!(err, PolicyStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_policy() {
        let s = store();
        s.put(fixture("p", AdmissionPolicyRules::default()))
            .await
            .unwrap();
        s.delete("p").await.unwrap();
        assert!(s.get("p").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn audit_banned_capability_emits_violation() {
        let s = store();
        let rules = AdmissionPolicyRules {
            banned_capabilities: vec!["filesystem_unrestricted".into()],
            ..Default::default()
        };
        s.put(fixture("baseline", rules)).await.unwrap();
        let plugins = vec![
            ctx_with("com.example.audio", &["respondent", "source"]),
            ctx_with(
                "com.untrusted.x",
                &["respondent", "filesystem_unrestricted"],
            ),
        ];
        let v = s
            .audit_plugins("baseline", &plugins, |_| true)
            .await
            .unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].plugin_name, "com.untrusted.x");
        assert_eq!(v[0].rule_class, "banned_capability");
    }

    #[tokio::test]
    async fn audit_capability_allowlist_rejects_outside_capabilities() {
        let s = store();
        let rules = AdmissionPolicyRules {
            allowed_capabilities: vec!["respondent".into(), "source".into()],
            ..Default::default()
        };
        s.put(fixture("strict", rules)).await.unwrap();
        let plugins = vec![
            ctx_with("com.example.audio", &["respondent", "source"]),
            ctx_with("com.example.x", &["respondent", "warden"]),
        ];
        let v = s.audit_plugins("strict", &plugins, |_| true).await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].plugin_name, "com.example.x");
        assert_eq!(v[0].rule_class, "capability_not_in_allowlist");
    }

    #[tokio::test]
    async fn audit_publisher_block_and_allowlist() {
        let s = store();
        let rules = AdmissionPolicyRules {
            ban_publishers: vec!["com.untrusted".into()],
            require_publisher_in: vec![
                "com.example".into(),
                "org.evoframework".into(),
            ],
            ..Default::default()
        };
        s.put(fixture("publisher", rules)).await.unwrap();
        let plugins = vec![
            ctx_with("com.example.audio", &["respondent"]),
            ctx_with("org.evoframework.x", &["source"]),
            ctx_with("com.untrusted.x", &["respondent"]),
            ctx_with("com.unknown.y", &["respondent"]),
        ];
        let v = s
            .audit_plugins("publisher", &plugins, |_| true)
            .await
            .unwrap();
        // com.untrusted.x → banned; com.unknown.y → not in
        // allowlist. The com.untrusted entry triggers BOTH
        // banned_publisher AND publisher_not_in_allowlist.
        assert!(v.iter().any(|x| x.plugin_name == "com.untrusted.x"
            && x.rule_class == "banned_publisher"));
        assert!(v.iter().any(|x| x.plugin_name == "com.unknown.y"
            && x.rule_class == "publisher_not_in_allowlist"));
    }

    #[tokio::test]
    async fn audit_require_verified_publisher_consults_reader() {
        let s = store();
        let rules = AdmissionPolicyRules {
            require_verified_publisher: true,
            ..Default::default()
        };
        s.put(fixture("verified", rules)).await.unwrap();
        let plugins = vec![
            ctx_with("com.verified.x", &["respondent"]),
            ctx_with("com.unverified.y", &["respondent"]),
        ];
        let v = s
            .audit_plugins("verified", &plugins, |publisher| {
                publisher == "com.verified"
            })
            .await
            .unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].plugin_name, "com.unverified.y");
        assert_eq!(v[0].rule_class, "publisher_not_verified");
    }

    #[tokio::test]
    async fn audit_clean_policy_returns_empty() {
        let s = store();
        s.put(fixture("clean", AdmissionPolicyRules::default()))
            .await
            .unwrap();
        let plugins = vec![ctx_with("com.example.x", &["respondent"])];
        let v = s.audit_plugins("clean", &plugins, |_| true).await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn audit_unverifiable_rules_emit_structural_violations() {
        let s = store();
        let rules = AdmissionPolicyRules {
            require_signed: true,
            max_outbound_network_endpoints: Some(5),
            max_total_plugin_memory_mb: Some(1024),
            ..Default::default()
        };
        s.put(fixture("strict", rules)).await.unwrap();
        let plugins = vec![ctx_with("com.example.x", &["respondent"])];
        let v = s.audit_plugins("strict", &plugins, |_| true).await.unwrap();
        // Three structural violations per plugin (one each for
        // require_signed, max_outbound, max_memory).
        assert_eq!(v.len(), 3);
        let classes: std::collections::HashSet<_> =
            v.iter().map(|x| x.rule_class.as_str()).collect();
        assert!(classes.contains("require_signed_unverifiable"));
        assert!(classes.contains("max_outbound_network_endpoints_unverifiable"));
        assert!(classes.contains("max_total_plugin_memory_mb_unverifiable"));
    }

    #[tokio::test]
    async fn audit_unknown_policy_errors_not_found() {
        let s = store();
        let err = s.audit_plugins("nope", &[], |_| true).await.unwrap_err();
        assert!(matches!(err, PolicyStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn rules_serde_round_trip_preserves_shape() {
        let rules = AdmissionPolicyRules {
            require_signed: true,
            require_verified_publisher: false,
            max_outbound_network_endpoints: Some(5),
            max_total_plugin_memory_mb: Some(1024),
            allowed_capabilities: vec!["respondent".into()],
            banned_capabilities: vec!["filesystem_unrestricted".into()],
            ban_publishers: vec!["com.untrusted".into()],
            require_publisher_in: vec![
                "com.example".into(),
                "org.evoframework".into(),
            ],
        };
        let s = serde_json::to_string(&rules).unwrap();
        let back: AdmissionPolicyRules = serde_json::from_str(&s).unwrap();
        assert_eq!(rules, back);
    }
}
