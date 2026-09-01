// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Immutable handle bag shared across the steward.
//!
//! Built once at boot, held as `Arc<StewardState>` by the components
//! that need a store handle (the dispatch layer, the server, plugin
//! routing, future admin paths). Each store inside is independently
//! synchronised; this type adds no locking of its own.
//!
//! `StewardState` is intentionally narrow: it holds the shared stores
//! the steward serves, not engine-internal scratch (admission tables,
//! correlation counters) or boot configuration (data roots, trust
//! state, security policy). Future refactors will route the existing
//! [`AdmissionEngine`](crate::admission::AdmissionEngine) through this
//! handle bag so concurrent client requests on different plugins no
//! longer serialise on a single mutex.
//!
//! The handle bag is shared by every store + engine in the steward;
//! `StewardState::builder` constructs it from the per-store
//! components admission and dispatch consume.

use std::sync::{Arc, OnceLock};

use evo_auth_bearer::{BearerTokenIssuer, CredentialStore, RevocationList};
use thiserror::Error;

use crate::admin::AdminLedger;
use crate::catalogue::Catalogue;
use crate::claimant::ClaimantTokenIssuer;
use crate::custody::CustodyLedger;
use crate::happenings::HappeningBus;
use crate::persistence::PersistenceStore;
use crate::projections::SubjectConflictIndex;
use crate::relations::RelationGraph;
use crate::subjects::SubjectRegistry;

/// Immutable bag of `Arc`-shared store handles used across the
/// steward.
///
/// Constructed via [`StewardState::builder`]; every field is
/// required. Once built, the bag is held as `Arc<StewardState>` and
/// cloned freely. Each contained store is independently synchronised
/// (each owns its own internal locking primitive); the bag itself
/// adds none.
///
/// Field set is the union of every shared store the
/// [`AdmissionEngine`](crate::admission::AdmissionEngine) carries
/// today plus the catalogue the steward loads at boot. Engine-private
/// state (admission tables, the custody-ID counter) and per-engine
/// boot configuration (the per-plugin data root, plugin-trust state,
/// per-class spawn identities) are deliberately excluded: the bag
/// describes shared stores, not dispatch or configuration.
///
/// `Debug` is implemented manually so the `bearer_token_issuer`
/// field's underlying ed25519 signing key never reaches a formatter.
/// The field renders as `<set>` / `<unset>` instead.
pub struct StewardState {
    /// The catalogue the steward administers. Loaded at boot and
    /// swappable by operator-issued reload_catalogue calls. Wrapped
    /// in [`arc_swap::ArcSwap`] so reads stay lock-free while a
    /// reload publishes a new declaration set atomically. Use
    /// [`Self::current_catalogue`] for ergonomic access.
    pub catalogue: arc_swap::ArcSwap<Catalogue>,
    /// The subject registry, implementing `SUBJECTS.md`. Plugins
    /// announce into it via their `LoadContext`; the projection
    /// engine and client query paths read from it.
    pub subjects: Arc<SubjectRegistry>,
    /// The relation graph, implementing `RELATIONS.md`. Plugins
    /// assert into it via their `LoadContext`; projections walk it
    /// with one-hop traversal.
    pub relations: Arc<RelationGraph>,
    /// The custody ledger. Tracks every custody handed out by the
    /// admission engine's custody verbs and every state report
    /// emitted by the holding warden.
    pub custody: Arc<CustodyLedger>,
    /// The happenings bus. Streams custody and (later) other
    /// fabric transitions to subscribers without polling.
    pub bus: Arc<HappeningBus>,
    /// The admin audit ledger. Records every privileged
    /// administration action an admitted admin plugin performs via
    /// the `SubjectAdmin` / `RelationAdmin` callbacks.
    pub admin: Arc<AdminLedger>,
    /// Durable backing store for the persistent slice of the
    /// fabric. The subject registry, custody ledger, relation graph,
    /// admin ledger, and happenings cursor all write through here;
    /// boot-time replay rehydrates each store from the persisted
    /// rows. The trait is held as a `dyn` reference so tests can
    /// substitute the in-memory mock.
    pub persistence: Arc<dyn PersistenceStore>,
    /// Issuer for claimant tokens. Held by every wire-emission site
    /// that needs to translate a plugin name into the consumer-
    /// facing token (the `From<Happening>` for `HappeningWire`
    /// conversion in `server.rs`, the projection-wire builders,
    /// etc.). Centralised here so all surfaces share one source of
    /// truth for the token derivation; the no-drift invariant
    /// requires every wire surface to mint tokens through this same
    /// issuer.
    pub claimant_issuer: Arc<ClaimantTokenIssuer>,
    /// In-memory conflict index that mirrors the durable
    /// `pending_conflicts` table. Held centrally so the wiring
    /// layer (announce path) records into it on detection, the
    /// admin layer (merge path) clears it on resolution, and the
    /// projection engine reads from it without an additional
    /// store query at composition time.
    pub conflict_index: Arc<SubjectConflictIndex>,
    /// Operator bearer-token issuer for the HTTPS / WS projection.
    /// Populated post-boot by the HTTPS substrate (see
    /// `https_boot::boot_https`) so the SSH-required operator-mint
    /// wire-op (`mint_bearer_token`) can sign tokens through the
    /// same per-device signing key that the WS layer's validator
    /// verifies against. `None` when HTTPS boot did not run or
    /// failed; the mint wire-op refuses with a descriptive error
    /// in that state.
    pub bearer_token_issuer: OnceLock<Arc<BearerTokenIssuer>>,
    /// Operator credential inventory store. Each minted
    /// bearer token persists a [`evo_auth_bearer::CredentialRecord`]
    /// here so the operator can list, revoke, and audit
    /// credentials through `list_bearer_tokens` /
    /// `revoke_bearer_token` / `reset_credentials_to_open`
    /// wire ops without depending on what tokens the operator
    /// happened to capture from the mint flow.
    pub credential_store: OnceLock<Arc<CredentialStore>>,
    /// Live revocation list the bearer-token validator
    /// consults on every request. Populated post-boot by the
    /// HTTPS substrate from `revoked.json` so revocations
    /// survive steward restarts; the `revoke_bearer_token`
    /// wire op writes through to both this list and the
    /// persistent file.
    pub revocation_list: OnceLock<Arc<RevocationList>>,
    /// Framework credential vault — the single per-plugin-scoped
    /// credential store the operator UI writes into via the
    /// `credential_put` / `credential_delete` / `credential_list_keys`
    /// wire ops and plugins read via `LoadContext.credential_vault`.
    /// Populated at boot when the steward wires
    /// `AdmissionEngine::with_credential_vault`; test harnesses
    /// leave it empty and the wire-op handlers surface a structured
    /// `vault_unavailable` refusal.
    pub credential_vault: OnceLock<Arc<crate::credentials::CredentialVault>>,
    /// Central per-plugin credential-change bus. Every admitted
    /// plugin has an entry keyed by canonical name; the operator-
    /// facing `credential_put` / `credential_delete` handlers
    /// publish on this bus after a successful vault mutation, and
    /// every currently-subscribed
    /// [`evo_plugin_sdk::contract::context::CredentialVaultHandle::subscribe_changes`]
    /// receiver (in-process reactor or OOP forwarder task)
    /// observes the event and re-resolves in place. Always
    /// populated (default-constructed) so the wire-op handlers
    /// never need an existence check.
    pub credential_change_bus: Arc<crate::credentials::CredentialChangeBus>,
    /// Per-online-provider config store — the operator's live
    /// per-provider enable/disable + priority state the multi-
    /// source metadata cascade consults. Populated at boot once
    /// the persistence layer is available; test harnesses leave
    /// it empty and wire-op handlers surface a structured
    /// `provider_store_unavailable` refusal.
    pub online_provider_config_store:
        OnceLock<Arc<crate::online_providers::OnlineProviderConfigStore>>,
    /// Global bus for online-provider config changes. The
    /// operator-facing set-enabled / set-priority wire-op
    /// handlers publish on this bus after a successful upsert;
    /// every plugin reactor consuming online-provider config
    /// subscribes and re-resolves its local view in place. Always
    /// populated (default-constructed).
    pub online_provider_config_bus:
        Arc<crate::online_providers::OnlineProviderConfigBus>,
}

impl StewardState {
    /// Begin building a `StewardState`.
    ///
    /// All fields are required; [`StewardStateBuilder::build`] returns
    /// [`StewardStateBuildError`] if any handle is missing. The
    /// builder is the only construction path so future store
    /// additions need not break callers: a new field becomes a new
    /// required setter and a new error variant.
    pub fn builder() -> StewardStateBuilder {
        StewardStateBuilder::default()
    }

    /// Snapshot the current catalogue. Cheap (an `Arc` clone via
    /// the underlying [`arc_swap::ArcSwap::load_full`]); callers
    /// hold the snapshot for the duration of one operation and see
    /// a consistent declaration set even if a concurrent
    /// reload_catalogue swaps it.
    pub fn current_catalogue(&self) -> Arc<Catalogue> {
        self.catalogue.load_full()
    }
}

impl std::fmt::Debug for StewardState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StewardState")
            .field("catalogue", &self.catalogue)
            .field("subjects", &self.subjects)
            .field("relations", &self.relations)
            .field("custody", &self.custody)
            .field("bus", &self.bus)
            .field("admin", &self.admin)
            .field("persistence", &self.persistence)
            .field("claimant_issuer", &self.claimant_issuer)
            .field("conflict_index", &self.conflict_index)
            .field(
                "bearer_token_issuer",
                &if self.bearer_token_issuer.get().is_some() {
                    "<set>"
                } else {
                    "<unset>"
                },
            )
            .field(
                "credential_store",
                &if self.credential_store.get().is_some() {
                    "<set>"
                } else {
                    "<unset>"
                },
            )
            .field(
                "revocation_list",
                &if self.revocation_list.get().is_some() {
                    "<set>"
                } else {
                    "<unset>"
                },
            )
            .finish()
    }
}

/// Builder for [`StewardState`].
///
/// Each setter is chainable and consumes `self`. Call
/// [`StewardStateBuilder::build`] to produce an `Arc<StewardState>`;
/// any unset field returns a [`StewardStateBuildError`] variant
/// identifying which handle was missing.
#[derive(Default)]
pub struct StewardStateBuilder {
    catalogue: Option<Arc<Catalogue>>,
    subjects: Option<Arc<SubjectRegistry>>,
    relations: Option<Arc<RelationGraph>>,
    custody: Option<Arc<CustodyLedger>>,
    bus: Option<Arc<HappeningBus>>,
    admin: Option<Arc<AdminLedger>>,
    persistence: Option<Arc<dyn PersistenceStore>>,
    claimant_issuer: Option<Arc<ClaimantTokenIssuer>>,
    conflict_index: Option<Arc<SubjectConflictIndex>>,
}

impl StewardStateBuilder {
    /// Provide the catalogue handle.
    pub fn catalogue(mut self, catalogue: Arc<Catalogue>) -> Self {
        self.catalogue = Some(catalogue);
        self
    }

    /// Provide the subject registry handle.
    pub fn subjects(mut self, subjects: Arc<SubjectRegistry>) -> Self {
        self.subjects = Some(subjects);
        self
    }

    /// Provide the relation graph handle.
    pub fn relations(mut self, relations: Arc<RelationGraph>) -> Self {
        self.relations = Some(relations);
        self
    }

    /// Provide the custody ledger handle.
    pub fn custody(mut self, custody: Arc<CustodyLedger>) -> Self {
        self.custody = Some(custody);
        self
    }

    /// Provide the happenings bus handle.
    pub fn bus(mut self, bus: Arc<HappeningBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Provide the admin audit ledger handle.
    pub fn admin(mut self, admin: Arc<AdminLedger>) -> Self {
        self.admin = Some(admin);
        self
    }

    /// Provide the persistence-store handle.
    pub fn persistence(
        mut self,
        persistence: Arc<dyn PersistenceStore>,
    ) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Provide the claimant-token issuer.
    pub fn claimant_issuer(mut self, issuer: Arc<ClaimantTokenIssuer>) -> Self {
        self.claimant_issuer = Some(issuer);
        self
    }

    /// Provide the in-memory conflict index. Optional: if omitted,
    /// the builder constructs an empty index so callers that do not
    /// rehydrate from persistence still get a valid handle.
    pub fn conflict_index(mut self, index: Arc<SubjectConflictIndex>) -> Self {
        self.conflict_index = Some(index);
        self
    }

    /// Finalise the builder.
    ///
    /// Returns `Arc<StewardState>` on success. Returns
    /// [`StewardStateBuildError`] if any required handle was not
    /// provided.
    pub fn build(self) -> Result<Arc<StewardState>, StewardStateBuildError> {
        Ok(Arc::new(StewardState {
            catalogue: arc_swap::ArcSwap::new(
                self.catalogue
                    .ok_or(StewardStateBuildError::MissingCatalogue)?,
            ),
            subjects: self
                .subjects
                .ok_or(StewardStateBuildError::MissingSubjects)?,
            relations: self
                .relations
                .ok_or(StewardStateBuildError::MissingRelations)?,
            custody: self
                .custody
                .ok_or(StewardStateBuildError::MissingCustody)?,
            bus: self.bus.ok_or(StewardStateBuildError::MissingBus)?,
            admin: self.admin.ok_or(StewardStateBuildError::MissingAdmin)?,
            persistence: self
                .persistence
                .ok_or(StewardStateBuildError::MissingPersistence)?,
            claimant_issuer: self
                .claimant_issuer
                .ok_or(StewardStateBuildError::MissingClaimantIssuer)?,
            conflict_index: self
                .conflict_index
                .unwrap_or_else(|| Arc::new(SubjectConflictIndex::new())),
            bearer_token_issuer: OnceLock::new(),
            credential_store: OnceLock::new(),
            revocation_list: OnceLock::new(),
            credential_vault: OnceLock::new(),
            credential_change_bus: Arc::new(
                crate::credentials::CredentialChangeBus::new(),
            ),
            online_provider_config_store: OnceLock::new(),
            online_provider_config_bus: Arc::new(
                crate::online_providers::OnlineProviderConfigBus::new(),
            ),
        }))
    }
}

#[cfg(test)]
impl StewardState {
    /// Build a `StewardState` populated with default-constructed
    /// stores and an empty catalogue, for tests that do not exercise
    /// catalogue-grammar specifics.
    ///
    /// Tests that need a specific catalogue use
    /// [`StewardState::for_tests_with_catalogue`] or the builder
    /// directly via [`StewardState::builder`].
    pub fn for_tests() -> Arc<Self> {
        Self::for_tests_with_catalogue(Arc::new(Catalogue::default()))
    }

    /// Like [`StewardState::for_tests`] but with a caller-supplied
    /// catalogue. Used by tests that exercise shelf-lookup,
    /// subject-type, or relation-predicate validation.
    pub fn for_tests_with_catalogue(catalogue: Arc<Catalogue>) -> Arc<Self> {
        Arc::new(Self {
            catalogue: arc_swap::ArcSwap::new(catalogue),
            subjects: Arc::new(SubjectRegistry::new()),
            relations: Arc::new(RelationGraph::new()),
            custody: Arc::new(CustodyLedger::new()),
            bus: Arc::new(HappeningBus::new()),
            admin: Arc::new(AdminLedger::new()),
            claimant_issuer: Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )),
            persistence: Arc::new(
                crate::persistence::MemoryPersistenceStore::new(),
            ),
            conflict_index: Arc::new(SubjectConflictIndex::new()),
            bearer_token_issuer: OnceLock::new(),
            credential_store: OnceLock::new(),
            revocation_list: OnceLock::new(),
            credential_vault: OnceLock::new(),
            credential_change_bus: Arc::new(
                crate::credentials::CredentialChangeBus::new(),
            ),
            online_provider_config_store: OnceLock::new(),
            online_provider_config_bus: Arc::new(
                crate::online_providers::OnlineProviderConfigBus::new(),
            ),
        })
    }
}

/// Errors returned by [`StewardStateBuilder::build`].
///
/// One variant per required field. Each variant names the missing
/// handle so the caller can correct the builder call site without
/// further inspection.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StewardStateBuildError {
    /// Builder was finalised without a catalogue handle.
    #[error("StewardState builder is missing the catalogue handle")]
    MissingCatalogue,
    /// Builder was finalised without a subject-registry handle.
    #[error("StewardState builder is missing the subject-registry handle")]
    MissingSubjects,
    /// Builder was finalised without a relation-graph handle.
    #[error("StewardState builder is missing the relation-graph handle")]
    MissingRelations,
    /// Builder was finalised without a custody-ledger handle.
    #[error("StewardState builder is missing the custody-ledger handle")]
    MissingCustody,
    /// Builder was finalised without a happenings-bus handle.
    #[error("StewardState builder is missing the happenings-bus handle")]
    MissingBus,
    /// Builder was finalised without an admin-ledger handle.
    #[error("StewardState builder is missing the admin-ledger handle")]
    MissingAdmin,
    /// Builder was finalised without a persistence-store handle.
    #[error("StewardState builder is missing the persistence-store handle")]
    MissingPersistence,
    /// Builder was finalised without a claimant-token-issuer handle.
    #[error(
        "StewardState builder is missing the claimant-token-issuer handle"
    )]
    MissingClaimantIssuer,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn memory_persistence() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryPersistenceStore::new())
    }

    fn full_builder() -> StewardStateBuilder {
        StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .subjects(Arc::new(SubjectRegistry::new()))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(memory_persistence())
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
    }

    #[test]
    fn builder_with_all_fields_returns_state() {
        let state = full_builder().build().expect("build should succeed");

        // Every field is reachable through the Arc<StewardState>.
        assert_eq!(state.current_catalogue().racks.len(), 0);
        // Touching the other handles is enough to prove the field is
        // populated; their semantics are covered in their own modules.
        let _ = Arc::clone(&state.subjects);
        let _ = Arc::clone(&state.relations);
        let _ = Arc::clone(&state.custody);
        let _ = Arc::clone(&state.bus);
        let _ = Arc::clone(&state.admin);
        let _ = Arc::clone(&state.persistence);
        let _ = Arc::clone(&state.claimant_issuer);
    }

    #[test]
    fn builder_missing_catalogue_returns_error() {
        let err = StewardState::builder()
            .subjects(Arc::new(SubjectRegistry::new()))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(memory_persistence())
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect_err("build without catalogue should fail");
        assert!(matches!(err, StewardStateBuildError::MissingCatalogue));
    }

    #[test]
    fn builder_missing_subjects_returns_error() {
        let err = StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(memory_persistence())
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect_err("build without subjects should fail");
        assert!(matches!(err, StewardStateBuildError::MissingSubjects));
    }

    #[test]
    fn builder_missing_relations_returns_error() {
        let err = StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .subjects(Arc::new(SubjectRegistry::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(memory_persistence())
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect_err("build without relations should fail");
        assert!(matches!(err, StewardStateBuildError::MissingRelations));
    }

    #[test]
    fn builder_missing_custody_returns_error() {
        let err = StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .subjects(Arc::new(SubjectRegistry::new()))
            .relations(Arc::new(RelationGraph::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(memory_persistence())
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect_err("build without custody should fail");
        assert!(matches!(err, StewardStateBuildError::MissingCustody));
    }

    #[test]
    fn builder_missing_bus_returns_error() {
        let err = StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .subjects(Arc::new(SubjectRegistry::new()))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(memory_persistence())
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect_err("build without bus should fail");
        assert!(matches!(err, StewardStateBuildError::MissingBus));
    }

    #[test]
    fn builder_missing_admin_returns_error() {
        let err = StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .subjects(Arc::new(SubjectRegistry::new()))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .persistence(memory_persistence())
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect_err("build without admin should fail");
        assert!(matches!(err, StewardStateBuildError::MissingAdmin));
    }

    #[test]
    fn builder_missing_persistence_returns_error() {
        let err = StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .subjects(Arc::new(SubjectRegistry::new()))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect_err("build without persistence should fail");
        assert!(matches!(err, StewardStateBuildError::MissingPersistence));
    }

    #[test]
    fn builder_missing_claimant_issuer_returns_error() {
        let err = StewardState::builder()
            .catalogue(Arc::new(Catalogue::default()))
            .subjects(Arc::new(SubjectRegistry::new()))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(memory_persistence())
            .build()
            .expect_err("build without claimant_issuer should fail");
        assert!(matches!(err, StewardStateBuildError::MissingClaimantIssuer));
    }

    #[test]
    fn state_handles_are_clonable_via_arc() {
        let state = full_builder().build().expect("build should succeed");

        // Capture an extra clone of the catalogue handle in the
        // spawned thread so we can observe that the bag is shareable
        // and that store handles stay alive across thread boundaries.
        let cloned = Arc::clone(&state);
        let handle = std::thread::spawn(move || {
            // Hold one Arc clone per store for the duration of this
            // thread; strong counts must include both this thread's
            // clones and the bag the parent still holds.
            let cat = cloned.current_catalogue();
            let _subjects = Arc::clone(&cloned.subjects);
            let _relations = Arc::clone(&cloned.relations);
            let _custody = Arc::clone(&cloned.custody);
            let _bus = Arc::clone(&cloned.bus);
            let _admin = Arc::clone(&cloned.admin);
            let _persistence = Arc::clone(&cloned.persistence);
            assert!(Arc::strong_count(&cat) >= 2);
        });
        handle.join().expect("spawned thread should complete");

        // The original bag is still usable after the spawned thread
        // has dropped its clones.
        let _ = state.current_catalogue();
    }
}
