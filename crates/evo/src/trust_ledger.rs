// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Domain-member trust ledger.
//!
//! A **domain** is the trust unit. A device joins the
//! domain once via operator gesture and remains
//! domain-resident across reboots, network outages, and
//! group lifecycle transitions until explicitly revoked.
//! Per-device admit + revoke is the granularity; revoke is
//! soft (the row is retained so the operator surface can
//! present "previously admitted" history and re-admit
//! without rebuilding state from scratch).
//!
//! The trust ledger is the framework-side source-of-truth for
//! domain roster. Operator-facing read paths
//! ([`crate::server`]'s `list_domain_members` wire op) and
//! group admission validation
//! ([`crate::groups::GroupStore::add_member`]) both project
//! over this primitive rather than reconstructing membership
//! from the union of discovery presence + group memberships +
//! local identity.
//!
//! Cross-device eventual consistency (when device A admits
//! device B, every other domain member's local ledger
//! gains the row) rides the durable subject-state subscriber
//! channel; this module owns the local-side persistence and
//! the local-side happening fan-out only.

use std::sync::Arc;

use crate::happenings::{Happening, HappeningBus};
use crate::persistence::{PersistedDomainMember, PersistenceError};

/// In-memory shape of one domain-membership row. Mirrors
/// [`PersistedDomainMember`] one-to-one.
pub type DomainMember = PersistedDomainMember;

/// Errors raised by [`TrustLedger`].
#[derive(Debug, thiserror::Error)]
pub enum TrustLedgerError {
    /// Underlying persistence error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Operator submitted an invalid display name (empty or
    /// excessively long) at admission time.
    #[error("display_name validation: {0}")]
    InvalidDisplayName(String),
}

/// Persistence-backed accessor for domain-membership rows.
///
/// Wraps an `Arc<dyn PersistenceStore>` and emits
/// `DomainMemberAdmitted` / `DomainMemberRevoked` /
/// `DomainMemberDisplayNameObserved` happenings on every
/// state-change.
///
/// When a [`crate::domain_witness::runtime::
/// DomainWitnessRuntime`] is bound via
/// [`Self::with_witness_runtime`] or
/// [`Self::set_witness_runtime`], read paths
/// ([`Self::list`], [`Self::get`], [`Self::is_admitted`])
/// project from the chain rather than the legacy
/// persistence table. The chain is the singular source of
/// truth for domain-member trust state; the persistence
/// table is retained for fallback reads when the chain
/// runtime is not yet bound (e.g. very early boot before
/// the witness runtime constructs).
///
/// The runtime is held in an [`arc_swap::ArcSwapOption`] so
/// it can be bound after the ledger is already wrapped in
/// an `Arc` — the construction order in `lib.rs::run` has
/// the trust ledger instantiated before the audio-plane
/// runtime that the chain runtime depends on.
pub struct TrustLedger {
    happenings: Arc<HappeningBus>,
    witness_runtime: arc_swap::ArcSwapOption<
        crate::domain_witness::runtime::DomainWitnessRuntime,
    >,
}

impl std::fmt::Debug for TrustLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustLedger").finish_non_exhaustive()
    }
}

impl TrustLedger {
    /// Construct a ledger wrapping the supplied happening bus.
    /// The chain runtime — which becomes the canonical read
    /// source — is bound via [`Self::set_witness_runtime`] /
    /// [`Self::with_witness_runtime`] once the boot path has
    /// constructed it.
    pub fn new(happenings: Arc<HappeningBus>) -> Self {
        Self {
            happenings,
            witness_runtime: arc_swap::ArcSwapOption::const_empty(),
        }
    }

    /// Bind a domain witness runtime so reads project from
    /// the chain. The chain is the source of truth for
    /// trust state in the substrate; this binding tells
    /// the ledger to use it. Consuming-builder shape for
    /// tests + early-boot scenarios that construct + bind
    /// in one expression.
    pub fn with_witness_runtime(
        self,
        runtime: Arc<crate::domain_witness::runtime::DomainWitnessRuntime>,
    ) -> Self {
        self.set_witness_runtime(runtime);
        self
    }

    /// Bind a domain witness runtime after the ledger has
    /// been wrapped in an `Arc`. Safe to call from any
    /// thread; the underlying `ArcSwapOption` performs the
    /// store lock-free.
    pub fn set_witness_runtime(
        &self,
        runtime: Arc<crate::domain_witness::runtime::DomainWitnessRuntime>,
    ) {
        self.witness_runtime.store(Some(runtime));
    }

    /// Translate a chain trust-projection row into the
    /// legacy `PersistedDomainMember` shape so call sites
    /// that expect the old type get a transparent
    /// substrate swap.
    fn projection_to_member(
        row: &crate::domain_witness::projection::TrustProjection,
    ) -> DomainMember {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        DomainMember {
            device_id: row.device_id.clone(),
            display_name: row.display_name.clone(),
            public_key_bytes: STANDARD.decode(&row.public_key_b64).ok(),
            admitted_at_ms: row.admitted_at_ns / 1_000_000,
            admitted_by_device_id: None,
            revoked_at_ms: row.discarded_at_ns.map(|ns| ns / 1_000_000),
        }
    }

    /// Admit a device to the domain.
    ///
    /// `device_id` becomes domain-resident. `display_name`
    /// seeds the persistent name cache (the framework returns
    /// this in the `list_domain_members` projection when
    /// the device is offline / quiet); the operator surface
    /// supplies the best name it has at admit time. Optional
    /// `public_key_bytes` carries vendor-supplied identity
    /// material.
    ///
    /// `admitted_by_device_id` records which device's operator
    /// UI initiated the admission. Pass `None` for the seed
    /// device admitting itself at first boot.
    ///
    /// Idempotent on already-admitted ids — re-admit overwrites
    /// the display-name cache + clears any soft-revoke. Fires
    /// a `DomainMemberAdmitted` happening unconditionally on
    /// success (the same shape the operator surface uses for
    /// initial admission and for re-admission of a previously
    /// revoked device).
    pub async fn admit(
        &self,
        device_id: &str,
        display_name: &str,
        public_key_bytes: Option<Vec<u8>>,
        admitted_by_device_id: Option<&str>,
    ) -> Result<DomainMember, TrustLedgerError> {
        let trimmed = display_name.trim();
        if trimmed.is_empty() {
            return Err(TrustLedgerError::InvalidDisplayName(
                "display_name must not be empty or whitespace-only".into(),
            ));
        }
        if trimmed.chars().count() > 128 {
            return Err(TrustLedgerError::InvalidDisplayName(format!(
                "display_name must be \u{2264} 128 chars (got {})",
                trimmed.chars().count()
            )));
        }
        let now_ms = now_ms();
        // The chain has already received the AdmitPeer witness
        // before this method runs — the caller (e.g.
        // `handle_admit_peer_to_domain`) appends it. This method
        // emits the happening so subscribers fan-out to UI and
        // composes the response shape; no parallel persistence
        // row is written, the chain projection is the canonical
        // record.
        let record = DomainMember {
            device_id: device_id.to_string(),
            display_name: trimmed.to_string(),
            public_key_bytes,
            admitted_at_ms: now_ms,
            admitted_by_device_id: admitted_by_device_id.map(|s| s.to_string()),
            revoked_at_ms: None,
        };
        self.emit(Happening::DomainMemberAdmitted {
            device_id: device_id.to_string(),
            display_name: trimmed.to_string(),
            admitted_at_ms: now_ms,
            admitted_by_device_id: admitted_by_device_id.map(|s| s.to_string()),
            at: std::time::SystemTime::now(),
        })
        .await;
        Ok(record)
    }

    /// Update the persistent display-name cache for an
    /// already-admitted domain member. Driven by discovery
    /// when a peer's mDNS-SD TXT record carries a newer name
    /// than the ledger holds.
    ///
    /// No-op when the row does not exist (the framework does
    /// not auto-admit on observation) or when the observed
    /// display_name matches the cached value (no real change).
    /// Emits `DomainMemberDisplayNameObserved` on real
    /// change.
    pub async fn observe_display_name(
        &self,
        device_id: &str,
        observed_display_name: &str,
    ) -> Result<bool, TrustLedgerError> {
        let trimmed = observed_display_name.trim();
        if trimmed.is_empty() {
            return Ok(false);
        }
        // Compare the mDNS-observed display_name to the
        // chain-anchored value. The chain projection is the
        // canonical record; an operator-issued
        // `RenamePeerDisplayName` chain witness is the only
        // way to change it. Discovery-observed divergence
        // emits the happening for UI subscribers (who can
        // surface "this device is advertising a different
        // name — confirm rename?") without rewriting the
        // chain or a persistence cache.
        let Some(row) = self.get(device_id).await? else {
            return Ok(false);
        };
        if row.display_name == trimmed {
            return Ok(false);
        }
        let now_ms = now_ms();
        self.emit(Happening::DomainMemberDisplayNameObserved {
            device_id: device_id.to_string(),
            display_name: trimmed.to_string(),
            observed_at_ms: now_ms,
            at: std::time::SystemTime::now(),
        })
        .await;
        Ok(true)
    }

    /// Fetch one domain-member row by canonical id.
    ///
    /// Reads from the chain projection when a witness
    /// runtime is bound; otherwise falls back to the
    /// legacy persistence table.
    pub async fn get(
        &self,
        device_id: &str,
    ) -> Result<Option<DomainMember>, TrustLedgerError> {
        let Some(runtime) = self.witness_runtime.load_full() else {
            // Pre-binding read returns None — the chain runtime
            // binds early in the boot path; consumers calling
            // before that window see an empty roster.
            return Ok(None);
        };
        let projection = runtime.current_projection();
        Ok(projection
            .trust
            .get(device_id)
            .map(Self::projection_to_member))
    }

    /// List every domain member, ordered by admission
    /// timestamp ascending. Includes revoked rows so the
    /// operator surface can present "previously admitted"
    /// history with a Revoked badge.
    ///
    /// Projects from the chain. Returns an empty list when
    /// the witness runtime is not yet bound (pre-binding
    /// boot window).
    pub async fn list(&self) -> Result<Vec<DomainMember>, TrustLedgerError> {
        let Some(runtime) = self.witness_runtime.load_full() else {
            return Ok(Vec::new());
        };
        let projection = runtime.current_projection();
        let mut rows: Vec<DomainMember> = projection
            .trust
            .values()
            .map(Self::projection_to_member)
            .collect();
        rows.sort_by_key(|r| r.admitted_at_ms);
        Ok(rows)
    }

    /// Returns `true` when the device id is a currently-admitted
    /// (non-revoked) domain member. Used by group-admission
    /// validation.
    pub async fn is_admitted(
        &self,
        device_id: &str,
    ) -> Result<bool, TrustLedgerError> {
        let Some(runtime) = self.witness_runtime.load_full() else {
            return Ok(false);
        };
        let projection = runtime.current_projection();
        Ok(projection.trust.get(device_id).is_some_and(|r| {
            r.state == crate::domain_witness::projection::TrustState::Admitted
        }))
    }

    async fn emit(&self, happening: Happening) {
        if let Err(e) = self.happenings.emit_durable(happening).await {
            tracing::warn!(
                error = %e,
                "trust ledger: emit happening failed"
            );
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> TrustLedger {
        TrustLedger::new(Arc::new(HappeningBus::new()))
    }

    /// Cross-seat chain-projection semantics (admit reaches
    /// other seats, list reflects the chain, revoke reads
    /// from projection) are covered end-to-end by the
    /// `tests/chain_three_seats.rs` integration suite +
    /// operator-verified on the three-rig fleet. The unit
    /// tests here cover input validation only — the boundary
    /// the chain layer doesn't enforce on its own.
    #[tokio::test]
    async fn admit_rejects_empty_display_name() {
        let l = ledger();
        let err = l
            .admit("device-A", "", None, None)
            .await
            .expect_err("empty must refuse");
        assert!(matches!(err, TrustLedgerError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn admit_rejects_overlong_display_name() {
        let l = ledger();
        let long_name = "a".repeat(129);
        let err = l
            .admit("device-A", &long_name, None, None)
            .await
            .expect_err("over-128 must refuse");
        assert!(matches!(err, TrustLedgerError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn list_returns_empty_without_witness_runtime() {
        let l = ledger();
        let rows = l.list().await.unwrap();
        assert!(
            rows.is_empty(),
            "pre-binding list returns empty — chain projection unbound"
        );
    }

    #[tokio::test]
    async fn is_admitted_returns_false_without_witness_runtime() {
        let l = ledger();
        assert!(!l.is_admitted("device-A").await.unwrap());
    }
}
