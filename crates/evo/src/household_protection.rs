// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Household protection — the operator's answer to "who in this
//! house may change what".
//!
//! This module owns the *mechanism* only:
//!
//! - the persisted policy object,
//! - the level ladder and what a level means in steward terms,
//! - the question the dispatch gate asks — "is scope `S`
//!   protected under this policy?",
//! - the seam a distribution uses to name its own Settings
//!   groups.
//!
//! It deliberately owns no product catalogue. There is no enum of
//! `file-sharing` / `metadata` / `smart-home`, and no distribution
//! scope name is compiled in. Group keys are opaque [`String`]s
//! that arrive from a distribution-supplied
//! [`HouseholdGroupTable`] on `RunOptions`, in the same family as
//! `RtcWakeCallback` and `HttpsSetup` (`BOUNDARY.md` §3). The test
//! that governs this split is `BOUNDARY.md` §5: *if an
//! `evo-device-<non-audio>` repository existed tomorrow, would
//! this change still make sense?* A lighting distribution has no
//! "file sharing" group, but it does have privileged scopes it
//! wants to keep with the owner — so the ladder is mechanism and
//! the group names are not.
//!
//! # Relationship to `AuthTier`
//!
//! None. [`AuthTier`](evo_runtime_http::auth_tier::AuthTier) is
//! API admission — whether a principal may reach the wire at all.
//! Household protection runs *after* admission and asks a
//! different question: this principal is admitted, but may it
//! mutate *this* scope right now. Overloading one on the other
//! would give the operator a single knob for two unrelated
//! concerns.
//!
//! # What is never locked
//!
//! Playback. Browsing, playing, pausing, skipping and volume are
//! never refused by this gate at any level, including `strict`
//! and including the `lend` overlay. "Play only" is the floor, not
//! a state the box can be argued out of. The default ladder below
//! therefore selects admin scopes exclusively, and a distribution
//! table that tried to protect a playback scope would still leave
//! the playback verbs admitting — the gate is consulted for
//! mutating settings verbs, not for transport.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evo_auth_bearer::{Capability, CapabilitySet};
use serde::{Deserialize, Serialize};

/// File name under `state_dir` holding the persisted policy.
///
/// Deliberately steward state, not browser state: the policy has
/// to bind a kiosk panel and a phone on the LAN identically, and
/// it has to survive a reinstall of the glass.
pub const POLICY_FILE_NAME: &str = "household-protection.json";

/// The operator's chosen protection level.
///
/// The wire spelling is the lowercase discriminant; the operator
/// wording lives in the UI, which reads the catalog from
/// `household_protection_get` rather than hard-coding a ladder of
/// its own.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionLevel {
    /// Home player. Anyone here can play and change everyday
    /// settings. No scope is protected.
    #[default]
    Open,
    /// Play and everyday settings; the owner keeps the networking
    /// and sharing surfaces.
    Low,
    /// Play and listen; settings stay with the owner.
    Standard,
    /// Play only.
    Strict,
}

impl ProtectionLevel {
    /// Rank on the ladder, `open` lowest. Used to decide whether a
    /// requested change *widens* admission (and therefore needs a
    /// step-up sitting) or narrows it (which never does — an
    /// operator handing the box to a guest must not be stopped).
    pub fn rank(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Low => 1,
            Self::Standard => 2,
            Self::Strict => 3,
        }
    }
}

/// The persisted policy object.
///
/// `chosen == false` is the first-start state: the box admits as
/// `open` while the operator reads the panel. That is deliberate —
/// a player that refuses to play until it has been paired is a
/// brick in the living room, and the household has physical
/// control of the device long before it has an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HouseholdProtectionPolicy {
    /// Whether the operator has completed the first-start choice.
    /// Until this is `true` the box admits as [`ProtectionLevel::Open`].
    #[serde(default)]
    pub chosen: bool,
    /// The chosen level.
    #[serde(default)]
    pub level: ProtectionLevel,
    /// The guest overlay. Playback stays; `protected_groups`
    /// refuse. Set and cleared without disturbing `level`.
    #[serde(default)]
    pub lend: bool,
    /// Owner-selected group keys to protect. Empty means "use the
    /// level's default groups". Keys are opaque to the steward and
    /// are only meaningful against the distribution's table.
    #[serde(default)]
    pub protected_groups: Vec<String>,
    /// The level in force before `lend` was raised, so unlocking
    /// restores the same object rather than inventing a second one.
    #[serde(default)]
    pub prior_level: Option<ProtectionLevel>,
}

impl Default for HouseholdProtectionPolicy {
    fn default() -> Self {
        Self {
            chosen: false,
            level: ProtectionLevel::Open,
            lend: false,
            protected_groups: Vec::new(),
            prior_level: None,
        }
    }
}

impl HouseholdProtectionPolicy {
    /// The level actually in force. Until the operator has chosen,
    /// the box behaves as `open` regardless of what is persisted.
    pub fn effective_level(&self) -> ProtectionLevel {
        if self.chosen {
            self.level
        } else {
            ProtectionLevel::Open
        }
    }

    /// Whether this policy locks anything at all.
    pub fn locks_anything(&self) -> bool {
        self.lend || self.effective_level() != ProtectionLevel::Open
    }

    /// Whether moving to `next` widens admission relative to
    /// `self` — leaving `lend`, or stepping down the ladder.
    ///
    /// Widening is what needs the device password. A guest must not
    /// be able to unlock the panel from the panel.
    pub fn widens_to(&self, next: &Self) -> bool {
        if self.lend && !next.lend {
            return true;
        }
        next.effective_level().rank() < self.effective_level().rank()
    }
}

/// The distribution's Settings-group table.
///
/// One object, supplied by the distribution — not by plugins.
/// Plugins never register groups; a distribution that wants a
/// group publishes it here so there is exactly one place to read
/// the map. The steward calls this only to answer "is scope `S`
/// protected", and never persists anything from it beyond the
/// opaque keys the operator selected.
pub trait HouseholdGroupTable: Send + Sync {
    /// Every group key this distribution defines, in the order the
    /// operator surface should offer them.
    fn group_keys(&self) -> Vec<String>;

    /// The capability scopes belonging to one group key. An
    /// unknown key yields an empty set — a stale owner selection
    /// must not panic the gate.
    fn scopes_for_group(&self, key: &str) -> Vec<String>;

    /// The group keys a level protects by default, before the
    /// operator adjusts the selection.
    fn groups_for_level(&self, level: ProtectionLevel) -> Vec<String>;
}

/// Shared handle to the distribution table, absent on the shipped
/// `evo` binary.
pub type GroupTable = Arc<dyn HouseholdGroupTable>;

/// The scopes a level protects when no distribution table is
/// wired — the shipped `evo` binary, and any image that has not
/// filled the hook.
///
/// This is mechanism, not a product catalogue: the ladder is
/// expressed in framework scope names already present on
/// [`operator_bootstrap_capability_set`](crate::https_boot::operator_bootstrap_capability_set),
/// plus a rank rule for `strict`. It exists so that `lend` means
/// something on a lighting image whose author has not yet written
/// a table — a level that silently protected nothing would be a
/// worse lie than a coarse one.
///
/// Playback scopes are absent by construction: every entry is an
/// admin scope.
fn default_protected_scopes(
    level: ProtectionLevel,
    bootstrap: &CapabilitySet,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match level {
        ProtectionLevel::Open => {}
        ProtectionLevel::Low => {
            out.insert("network_admin".to_owned());
        }
        ProtectionLevel::Standard => {
            out.insert("network_admin".to_owned());
            out.insert("system_admin".to_owned());
            out.insert("plugins_admin".to_owned());
        }
        ProtectionLevel::Strict => {
            // Every step-up admin scope the bootstrap set carries.
            // Rank is the selector, so a distribution that adds its
            // own step-up scope is covered without naming it here.
            for cap in bootstrap.capabilities() {
                if matches!(cap, Capability::StepUp { .. }) {
                    out.insert(cap.scope().to_owned());
                }
            }
        }
    }
    out
}

/// Resolve the set of capability scopes this policy protects.
///
/// With a distribution table wired, the owner's `protected_groups`
/// (or the level's default groups when the owner has not adjusted
/// the selection) expand through the table. Without one, the
/// built-in ladder above applies.
///
/// `lend` does not change *which* scopes are protected — it is an
/// overlay that keeps playback and applies the same protected set.
/// That is why leaving `lend` restores the prior object exactly.
pub fn protected_scopes(
    policy: &HouseholdProtectionPolicy,
    table: Option<&GroupTable>,
    bootstrap: &CapabilitySet,
) -> BTreeSet<String> {
    let level = policy.effective_level();
    if !policy.lend && level == ProtectionLevel::Open {
        return BTreeSet::new();
    }
    let Some(table) = table else {
        // No table: the level ladder still means something.
        let effective = if policy.lend && level == ProtectionLevel::Open {
            // Lending an otherwise-open box still keeps the owner's
            // privileged surfaces to the owner.
            ProtectionLevel::Low
        } else {
            level
        };
        return default_protected_scopes(effective, bootstrap);
    };

    let mut keys: Vec<String> = if policy.protected_groups.is_empty() {
        table.groups_for_level(level)
    } else {
        policy.protected_groups.clone()
    };

    // The lend floor. While lending, the owner's marks may add above
    // `low` but may never subtract below it: a lend that protected
    // nothing would be a lie in the operator's hand. Applied on read
    // as well as on store so a hand-edited file cannot sink under it.
    if policy.lend {
        for key in table.groups_for_level(ProtectionLevel::Low) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }

    // `*` means "every group this distribution defines". `strict`
    // uses it so a group added later is covered without anyone
    // remembering to restate the level defaults.
    if keys.iter().any(|k| k == ALL_GROUPS) {
        keys = table.group_keys();
    }

    let mut out = BTreeSet::new();
    for key in keys {
        for scope in table.scopes_for_group(&key) {
            out.insert(scope);
        }
    }
    out
}

/// Normalise a policy for storage.
///
/// `household_protection_set` persists the *result* of this, never
/// the raw body: a stored `lend: true` with empty marks would let a
/// later read disagree with the gate. Concretely — while lending,
/// the owner's marks are unioned with the `low` floor, so
/// "lend with nothing marked" becomes "lend with `low` marked"
/// on disk and in the snapshot the surface paints.
///
/// With no table wired there are no group keys to fill; the
/// absent-hook ladder in [`protected_scopes`] supplies the same
/// floor from scope names instead.
pub fn normalise_for_store(
    policy: &HouseholdProtectionPolicy,
    table: Option<&GroupTable>,
) -> HouseholdProtectionPolicy {
    let mut out = policy.clone();
    if !out.lend {
        return out;
    }
    let Some(table) = table else {
        return out;
    };
    let mut keys = if out.protected_groups.is_empty() {
        table.groups_for_level(out.effective_level())
    } else {
        out.protected_groups.clone()
    };
    if keys.iter().any(|k| k == ALL_GROUPS) {
        keys = table.group_keys();
    }
    for key in table.groups_for_level(ProtectionLevel::Low) {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    out.protected_groups = keys;
    out
}

/// The question the dispatch gate asks.
pub fn is_scope_protected(
    policy: &HouseholdProtectionPolicy,
    table: Option<&GroupTable>,
    bootstrap: &CapabilitySet,
    scope: &str,
) -> bool {
    protected_scopes(policy, table, bootstrap).contains(scope)
}

/// Wire spelling of every level, in ladder order.
pub const LEVEL_IDS: [(ProtectionLevel, &str); 4] = [
    (ProtectionLevel::Open, "open"),
    (ProtectionLevel::Low, "low"),
    (ProtectionLevel::Standard, "standard"),
    (ProtectionLevel::Strict, "strict"),
];

/// Group-key wildcard: "every group id in the catalog".
///
/// `strict` uses this so a distribution does not have to restate
/// its whole group list, and so a group added later is covered
/// without touching the level defaults.
pub const ALL_GROUPS: &str = "*";

/// One level row in the catalog handed to the operator surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogLevel {
    /// Wire spelling of the level.
    pub id: String,
    /// Group ids this level protects by default. May be the single
    /// entry [`ALL_GROUPS`].
    pub default_groups: Vec<String>,
}

/// One group row in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogGroup {
    /// Opaque group id, owned by the distribution.
    pub id: String,
    /// Capability scopes this group covers.
    pub scopes: Vec<String>,
}

/// The catalog handed to the operator surface alongside the policy,
/// so the UI performs one read and keeps no second map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCatalog {
    /// Ladder rows, in order.
    pub levels: Vec<CatalogLevel>,
    /// Group rows. Empty when no distribution table is wired.
    pub groups: Vec<CatalogGroup>,
}

/// The wire snapshot returned by `household_protection_get`, by a
/// successful `household_protection_set`, and carried (minus the
/// catalog) on the change happening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HouseholdProtectionSnapshot {
    /// Constant `true` — lets a surface detect a steward that
    /// carries this row at all without version sniffing.
    pub household_protection: bool,
    /// Whether the operator has completed the first-start choice.
    pub chosen: bool,
    /// The chosen level.
    pub level: ProtectionLevel,
    /// The guest overlay.
    pub lend: bool,
    /// Owner-selected group ids.
    pub protected_groups: Vec<String>,
    /// Level in force before `lend` was raised.
    pub prior_level: Option<ProtectionLevel>,
    /// Level ladder + group map, from the distribution table when
    /// one is wired.
    pub catalog: GroupCatalog,
}

/// Build the catalog for the current table. With no table wired the
/// group list is empty and the ladder still ships, so a surface can
/// paint levels on an image whose author has not written a table.
pub fn group_catalog(table: Option<&GroupTable>) -> GroupCatalog {
    match table {
        None => GroupCatalog {
            levels: LEVEL_IDS
                .iter()
                .map(|(_, id)| CatalogLevel {
                    id: (*id).to_owned(),
                    default_groups: Vec::new(),
                })
                .collect(),
            groups: Vec::new(),
        },
        Some(table) => GroupCatalog {
            levels: LEVEL_IDS
                .iter()
                .map(|(level, id)| CatalogLevel {
                    id: (*id).to_owned(),
                    default_groups: table.groups_for_level(*level),
                })
                .collect(),
            groups: table
                .group_keys()
                .into_iter()
                .map(|key| CatalogGroup {
                    scopes: table.scopes_for_group(&key),
                    id: key,
                })
                .collect(),
        },
    }
}

/// Assemble the wire snapshot from a policy plus the current table.
pub fn snapshot(
    policy: &HouseholdProtectionPolicy,
    table: Option<&GroupTable>,
) -> HouseholdProtectionSnapshot {
    HouseholdProtectionSnapshot {
        household_protection: true,
        chosen: policy.chosen,
        level: policy.level,
        lend: policy.lend,
        protected_groups: policy.protected_groups.clone(),
        prior_level: policy.prior_level,
        catalog: group_catalog(table),
    }
}

/// Absolute path of the persisted policy under `state_dir`.
pub fn policy_path(state_dir: &Path) -> PathBuf {
    state_dir.join(POLICY_FILE_NAME)
}

/// Directory that holds [`POLICY_FILE_NAME`], derived from
/// [`crate::config::PersistenceSection::path`].
///
/// That config field is the SQLite *file* (`…/state/evo.db`). The
/// policy is a sibling JSON in the same directory, not a child of
/// the database file. Passing the file path into
/// [`store_policy`] makes `create_dir_all` return EEXIST
/// (`household_protection_not_persisted`) on every real box.
pub fn state_dir_from_persistence_path(persistence_path: &Path) -> PathBuf {
    persistence_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Load the policy, returning the first-start default when the file
/// is absent or unreadable.
///
/// A corrupt file is *not* a reason to fail closed into a locked
/// box the operator cannot unlock from the panel; it is a reason to
/// return to first start, where the operator is asked again.
pub fn load_policy(state_dir: &Path) -> HouseholdProtectionPolicy {
    let path = policy_path(state_dir);
    let Ok(bytes) = std::fs::read(&path) else {
        return HouseholdProtectionPolicy::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        // Discarded, not silently: missing and corrupt take the same
        // admit path (first start, open, modal on first paint), but a
        // discarded file is an operator-visible event. No third
        // ceremony — an attacker who can write state_dir is already
        // past this file.
        tracing::error!(
            path = %path.display(),
            %error,
            "household protection policy discarded as unreadable; \
             returning to first start"
        );
        HouseholdProtectionPolicy::default()
    })
}

/// Persist the policy atomically (write-and-rename), so a power cut
/// mid-write cannot leave a half-parsed policy behind.
pub fn store_policy(
    state_dir: &Path,
    policy: &HouseholdProtectionPolicy,
) -> std::io::Result<()> {
    let path = policy_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body =
        serde_json::to_vec_pretty(policy).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// Shared runtime handle: the loaded policy, the distribution
/// table, and everything the wire ops and the dispatch gate need to
/// answer without touching disk on the hot path.
///
/// One instance per steward, constructed at boot and shared. Reads
/// take the read lock; only `household_protection_set` takes the
/// write lock, and it persists the normalised policy before it
/// publishes the change.
#[derive(Debug)]
pub struct HouseholdProtectionRuntime {
    state_dir: PathBuf,
    table: Option<GroupTable>,
    bootstrap: CapabilitySet,
    policy: std::sync::RwLock<HouseholdProtectionPolicy>,
}

impl std::fmt::Debug for dyn HouseholdGroupTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HouseholdGroupTable")
    }
}

impl HouseholdProtectionRuntime {
    /// Load the persisted policy (or first-start default) and build
    /// the shared handle.
    pub fn load(
        state_dir: impl Into<PathBuf>,
        table: Option<GroupTable>,
        bootstrap: CapabilitySet,
    ) -> Self {
        let state_dir = state_dir.into();
        let policy = load_policy(&state_dir);
        Self {
            state_dir,
            table,
            bootstrap,
            policy: std::sync::RwLock::new(policy),
        }
    }

    /// Current policy, cloned.
    pub fn policy(&self) -> HouseholdProtectionPolicy {
        self.policy.read().map(|p| p.clone()).unwrap_or_default()
    }

    /// The wire snapshot for `household_protection_get`.
    pub fn snapshot(&self) -> HouseholdProtectionSnapshot {
        snapshot(&self.policy(), self.table.as_ref())
    }

    /// The question the dispatch gate asks.
    pub fn is_scope_protected(&self, scope: &str) -> bool {
        is_scope_protected(
            &self.policy(),
            self.table.as_ref(),
            &self.bootstrap,
            scope,
        )
    }

    /// Whether moving to `next` widens admission and therefore needs
    /// a step-up sitting.
    pub fn widens_to(&self, next: &HouseholdProtectionPolicy) -> bool {
        self.policy().widens_to(next)
    }

    /// Apply a set body: normalise, persist, swap in, and hand back
    /// the post-write snapshot for the caller to return and publish.
    ///
    /// `chosen` is forced true — a successful set is the first-start
    /// choice being made.
    pub fn apply(
        &self,
        mut next: HouseholdProtectionPolicy,
    ) -> std::io::Result<HouseholdProtectionSnapshot> {
        next.chosen = true;
        let current = self.policy();
        // Remember where to return to when the lend is lifted, so
        // unlock restores the same object rather than inventing one.
        if next.lend && !current.lend {
            next.prior_level = Some(current.effective_level());
        }
        if !next.lend {
            next.prior_level = None;
        }
        let normalised = normalise_for_store(&next, self.table.as_ref());
        store_policy(&self.state_dir, &normalised)?;
        if let Ok(mut guard) = self.policy.write() {
            *guard = normalised;
        }
        Ok(self.snapshot())
    }

    /// The level to fall back to when a set body omits `level` while
    /// lifting a lend — the level in force before the lend.
    pub fn restore_level(&self) -> ProtectionLevel {
        let policy = self.policy();
        policy.prior_level.unwrap_or(policy.level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::https_boot::operator_bootstrap_capability_set;

    /// Fixture table. Tests inject this; they do not depend on
    /// evo-device-audio, and the steward never learns these names.
    struct FixtureTable;

    impl HouseholdGroupTable for FixtureTable {
        fn group_keys(&self) -> Vec<String> {
            vec!["alpha".to_owned(), "beta".to_owned()]
        }
        fn scopes_for_group(&self, key: &str) -> Vec<String> {
            match key {
                "alpha" => vec!["network_admin".to_owned()],
                "beta" => vec!["system_admin".to_owned()],
                _ => Vec::new(),
            }
        }
        fn groups_for_level(&self, level: ProtectionLevel) -> Vec<String> {
            match level {
                ProtectionLevel::Open => Vec::new(),
                ProtectionLevel::Low => vec!["alpha".to_owned()],
                ProtectionLevel::Standard | ProtectionLevel::Strict => {
                    vec!["alpha".to_owned(), "beta".to_owned()]
                }
            }
        }
    }

    fn table() -> GroupTable {
        Arc::new(FixtureTable)
    }

    #[test]
    fn first_start_admits_as_open() {
        let policy = HouseholdProtectionPolicy::default();
        assert!(!policy.chosen);
        assert_eq!(policy.effective_level(), ProtectionLevel::Open);
        assert!(!policy.locks_anything());
        let boot = operator_bootstrap_capability_set();
        assert!(protected_scopes(&policy, Some(&table()), &boot).is_empty());
    }

    #[test]
    fn persisted_level_is_inert_until_chosen() {
        let policy = HouseholdProtectionPolicy {
            chosen: false,
            level: ProtectionLevel::Strict,
            ..Default::default()
        };
        assert_eq!(policy.effective_level(), ProtectionLevel::Open);
        let boot = operator_bootstrap_capability_set();
        assert!(protected_scopes(&policy, Some(&table()), &boot).is_empty());
    }

    #[test]
    fn owner_selection_overrides_level_defaults() {
        let boot = operator_bootstrap_capability_set();
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Standard,
            protected_groups: vec!["beta".to_owned()],
            ..Default::default()
        };
        let scopes = protected_scopes(&policy, Some(&table()), &boot);
        assert!(scopes.contains("system_admin"));
        assert!(!scopes.contains("network_admin"));
    }

    #[test]
    fn unknown_group_key_is_inert_not_fatal() {
        let boot = operator_bootstrap_capability_set();
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Low,
            protected_groups: vec!["gone-in-an-upgrade".to_owned()],
            ..Default::default()
        };
        assert!(protected_scopes(&policy, Some(&table()), &boot).is_empty());
    }

    #[test]
    fn absent_table_still_gives_the_ladder_meaning() {
        let boot = operator_bootstrap_capability_set();
        let low = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Low,
            ..Default::default()
        };
        let scopes = protected_scopes(&low, None, &boot);
        assert!(scopes.contains("network_admin"));
        assert!(!scopes.contains("system_admin"));

        let standard = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Standard,
            ..Default::default()
        };
        let scopes = protected_scopes(&standard, None, &boot);
        for expected in ["network_admin", "system_admin", "plugins_admin"] {
            assert!(scopes.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn absent_table_strict_covers_every_step_up_admin_scope() {
        let boot = operator_bootstrap_capability_set();
        let strict = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Strict,
            ..Default::default()
        };
        let scopes = protected_scopes(&strict, None, &boot);
        for cap in boot.capabilities() {
            if matches!(cap, Capability::StepUp { .. }) {
                assert!(
                    scopes.contains(cap.scope()),
                    "strict missed step-up scope {}",
                    cap.scope()
                );
            }
        }
    }

    #[test]
    fn no_playback_scope_is_ever_protected_by_the_default_ladder() {
        let boot = operator_bootstrap_capability_set();
        // Read scopes and the plain write scopes the transport
        // surface rides on must never appear on the default ladder.
        for level in [
            ProtectionLevel::Low,
            ProtectionLevel::Standard,
            ProtectionLevel::Strict,
        ] {
            let policy = HouseholdProtectionPolicy {
                chosen: true,
                level,
                ..Default::default()
            };
            let scopes = protected_scopes(&policy, None, &boot);
            // Transport, not admin. `play` / `pause` / `set_volume` /
            // `browse_library` declare no verb_capabilities at all, so
            // they ride these scopes. `audio_admin` is DAC/DSP and is
            // deliberately NOT on this list — strict locking it is
            // correct.
            for scope in ["audio", "subjects", "request"] {
                assert!(
                    !scopes.contains(scope),
                    "{level:?} protected transport scope {scope}"
                );
            }
            for cap in boot.capabilities() {
                if matches!(cap, Capability::Read { .. }) {
                    assert!(
                        !scopes.contains(cap.scope()),
                        "{level:?} protected read scope {}",
                        cap.scope()
                    );
                }
            }
        }
    }

    #[test]
    fn lend_on_an_open_box_protects_the_low_set() {
        let boot = operator_bootstrap_capability_set();
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Open,
            lend: true,
            ..Default::default()
        };
        assert!(policy.locks_anything());
        // The invariant is that the scope is PROTECTED — the
        // principal does not keep it. Do not flip this.
        assert!(is_scope_protected(&policy, None, &boot, "network_admin"));
    }

    #[test]
    fn leaving_lend_widens_and_lowering_the_ladder_widens() {
        let lent = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Standard,
            lend: true,
            ..Default::default()
        };
        let unlent = HouseholdProtectionPolicy {
            lend: false,
            ..lent.clone()
        };
        assert!(lent.widens_to(&unlent));

        let strict = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Strict,
            ..Default::default()
        };
        let open = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Open,
            ..Default::default()
        };
        assert!(strict.widens_to(&open));
        assert!(
            !open.widens_to(&strict),
            "narrowing must never need step-up"
        );
    }

    #[test]
    fn lend_with_no_marks_is_stored_filled_not_empty() {
        let t = table();
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Open,
            lend: true,
            ..Default::default()
        };
        let stored = normalise_for_store(&policy, Some(&t));
        assert!(
            !stored.protected_groups.is_empty(),
            "lend + empty marks must never reach disk"
        );
        assert!(stored.protected_groups.contains(&"alpha".to_owned()));
    }

    #[test]
    fn lend_marks_may_add_above_the_floor_but_not_sink_below_it() {
        let t = table();
        let boot = operator_bootstrap_capability_set();
        // Owner marks only "beta" (above the low floor) while lending.
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Open,
            lend: true,
            protected_groups: vec!["beta".to_owned()],
            ..Default::default()
        };
        let stored = normalise_for_store(&policy, Some(&t));
        assert!(stored.protected_groups.contains(&"beta".to_owned()));
        assert!(
            stored.protected_groups.contains(&"alpha".to_owned()),
            "low floor must survive an owner selection that omits it"
        );
        // And the gate agrees on read, even for a hand-edited file
        // that never went through the normaliser.
        assert!(is_scope_protected(
            &policy,
            Some(&t),
            &boot,
            "network_admin"
        ));
        assert!(is_scope_protected(&policy, Some(&t), &boot, "system_admin"));
    }

    #[test]
    fn not_lending_leaves_marks_exactly_as_the_owner_set_them() {
        let t = table();
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Standard,
            lend: false,
            protected_groups: vec!["beta".to_owned()],
            ..Default::default()
        };
        let stored = normalise_for_store(&policy, Some(&t));
        assert_eq!(stored.protected_groups, vec!["beta".to_owned()]);
    }

    #[test]
    fn catalog_without_a_table_offers_no_groups_but_keeps_the_ladder() {
        let catalog = group_catalog(None);
        assert!(catalog.groups.is_empty());
        let ids: Vec<&str> =
            catalog.levels.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec!["open", "low", "standard", "strict"]);
    }

    #[test]
    fn catalog_with_a_table_carries_offer_order_and_scopes() {
        let t = table();
        let catalog = group_catalog(Some(&t));
        let ids: Vec<&str> =
            catalog.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "beta"]);
        assert_eq!(catalog.groups[0].scopes, vec!["network_admin"]);
    }

    #[test]
    fn snapshot_carries_the_constant_marker_and_the_catalog() {
        let t = table();
        let policy = HouseholdProtectionPolicy::default();
        let snap = snapshot(&policy, Some(&t));
        assert!(snap.household_protection);
        assert!(!snap.chosen);
        assert_eq!(snap.level, ProtectionLevel::Open);
        assert!(snap.prior_level.is_none());
        assert_eq!(snap.catalog.groups.len(), 2);
    }

    #[test]
    fn wildcard_level_default_expands_to_every_group() {
        struct StarTable;
        impl HouseholdGroupTable for StarTable {
            fn group_keys(&self) -> Vec<String> {
                vec!["one".to_owned(), "two".to_owned()]
            }
            fn scopes_for_group(&self, key: &str) -> Vec<String> {
                match key {
                    "one" => vec!["network_admin".to_owned()],
                    "two" => vec!["system_admin".to_owned()],
                    _ => Vec::new(),
                }
            }
            fn groups_for_level(&self, level: ProtectionLevel) -> Vec<String> {
                match level {
                    ProtectionLevel::Strict => vec![ALL_GROUPS.to_owned()],
                    _ => Vec::new(),
                }
            }
        }
        let t: GroupTable = Arc::new(StarTable);
        let boot = operator_bootstrap_capability_set();
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Strict,
            ..Default::default()
        };
        let scopes = protected_scopes(&policy, Some(&t), &boot);
        assert!(scopes.contains("network_admin"));
        assert!(scopes.contains("system_admin"));
    }

    #[test]
    fn snapshot_serialises_to_the_frozen_wire_shape() {
        let t = table();
        let policy = HouseholdProtectionPolicy::default();
        let value = serde_json::to_value(snapshot(&policy, Some(&t))).unwrap();
        assert_eq!(value["household_protection"], serde_json::json!(true));
        assert_eq!(value["chosen"], serde_json::json!(false));
        assert_eq!(value["level"], serde_json::json!("open"));
        assert_eq!(value["lend"], serde_json::json!(false));
        assert_eq!(value["protected_groups"], serde_json::json!([]));
        assert_eq!(value["prior_level"], serde_json::Value::Null);
        assert!(value["catalog"]["levels"].is_array());
        assert!(value["catalog"]["groups"].is_array());
        assert_eq!(
            value["catalog"]["levels"][0]["id"],
            serde_json::json!("open")
        );
        assert_eq!(
            value["catalog"]["groups"][0]["id"],
            serde_json::json!("alpha")
        );
    }

    #[test]
    fn policy_round_trips_through_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let policy = HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Standard,
            lend: true,
            protected_groups: vec!["alpha".to_owned()],
            prior_level: Some(ProtectionLevel::Low),
        };
        store_policy(dir.path(), &policy).unwrap();
        assert_eq!(load_policy(dir.path()), policy);
    }

    #[test]
    fn persistence_path_is_the_sqlite_file_policy_is_the_sibling() {
        assert_eq!(
            state_dir_from_persistence_path(Path::new(
                "/var/lib/evo/state/evo.db"
            )),
            PathBuf::from("/var/lib/evo/state")
        );
        assert_eq!(
            state_dir_from_persistence_path(Path::new("evo.db")),
            PathBuf::from(".")
        );
    }

    #[test]
    fn apply_persists_beside_an_existing_sqlite_file() {
        // The live box already has evo.db. Passing that file as
        // state_dir is what produced household_protection_not_persisted
        // (EEXIST) on first Save. The sibling directory must work.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("evo.db");
        std::fs::write(&db, b"not a real sqlite").unwrap();
        let state_dir = state_dir_from_persistence_path(&db);
        let runtime = HouseholdProtectionRuntime::load(
            state_dir,
            Some(table()),
            operator_bootstrap_capability_set(),
        );
        let snap = runtime
            .apply(HouseholdProtectionPolicy {
                chosen: true,
                level: ProtectionLevel::Open,
                ..Default::default()
            })
            .expect("persist beside evo.db");
        assert!(snap.chosen);
        assert!(dir.path().join(POLICY_FILE_NAME).is_file());
        assert!(db.is_file(), "sqlite file must stay a file");
    }

    #[test]
    fn corrupt_policy_returns_to_first_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(policy_path(dir.path()), b"{ not json").unwrap();
        let loaded = load_policy(dir.path());
        assert!(!loaded.chosen);
        assert_eq!(loaded.effective_level(), ProtectionLevel::Open);
    }
}
