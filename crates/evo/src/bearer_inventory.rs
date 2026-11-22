// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Every bearer the steward issues goes into the operator inventory.
//!
//! The inventory is the [`CredentialStore`]: one record per bearer,
//! whatever issued it — `create_bearer_token`, the first-boot
//! bootstrap token, pairing, the kiosk panel, or `mint_bearer_token`
//! on the local socket. `list_bearer_tokens` lists it and
//! `revoke_bearer_token` revokes through it, so a bearer with no record
//! could be neither listed nor revoked. Each path therefore writes its
//! record before it hands the bearer out, and refuses rather than hand
//! out a bearer whose record could not be written.
//!
//! Records whose bearers have expired are dropped at boot, and when a
//! record is added, at most once every [`PRUNE_INTERVAL`]: the
//! validator refuses an expired bearer anyway, and the kiosk panel and
//! paired browsers are issued a bearer a day. Between those scans a
//! record is added without reading the inventory, so a mint costs the
//! same however many records it holds.

use evo_auth_bearer::{
    CapabilityRef, CapabilitySet, CredentialRecord, CredentialSource,
    CredentialStore, CredentialStoreError, ExpiryPolicy,
};
use std::time::{Duration, Instant};

/// How often adding records may scan the inventory for expired ones.
pub(crate) const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// The facts of one issued bearer the record describes.
pub(crate) struct Issued<'a> {
    /// The bearer's id.
    pub token_id: &'a str,
    /// What the bearer may do.
    pub capabilities: &'a CapabilitySet,
    /// When it was issued, in ms since the Unix epoch.
    pub issued_at_ms: u64,
    /// When it expires, in ms since the Unix epoch.
    pub expires_at_ms: u64,
}

/// The record for `issued`, named `name`, with the reason it was
/// issued and the path that issued it.
pub(crate) fn issued_record(
    issued: Issued<'_>,
    name: &str,
    reason: &str,
    source: CredentialSource,
    paired_device_id: Option<&str>,
) -> CredentialRecord {
    CredentialRecord {
        token_id: issued.token_id.to_string(),
        name: name.to_string(),
        created_reason: reason.to_string(),
        scopes: issued
            .capabilities
            .capabilities()
            .iter()
            .map(CapabilityRef::from)
            .collect(),
        expiry_policy: ExpiryPolicy::Seconds {
            value: issued.expires_at_ms.saturating_sub(issued.issued_at_ms)
                / 1000,
        },
        created_at_ms: issued.issued_at_ms,
        expires_at_ms: Some(issued.expires_at_ms),
        revoked_at_ms: None,
        revoked_reason: None,
        source: Some(source),
        paired_device_id: paired_device_id.map(str::to_string),
    }
}

/// Add `record` to the inventory, first dropping the records whose
/// bearers have expired by `now_ms` when the inventory has not been
/// scanned in the last [`PRUNE_INTERVAL`].
///
/// Only the write of `record` decides the outcome: a failed drop
/// leaves an expired record listed, which a later addition or the next
/// boot drops, so it is logged and the addition goes ahead.
pub(crate) fn add_record(
    store: &CredentialStore,
    record: &CredentialRecord,
    now_ms: u64,
) -> Result<(), CredentialStoreError> {
    if let Err(e) = store.prune_expired_at_most_every(
        now_ms,
        Instant::now(),
        PRUNE_INTERVAL,
    ) {
        // LOGGING.md §2: warn (recoverable anomaly — an expired record
        // stays listed until the next addition or boot drops it; the
        // bearer it describes is refused regardless).
        tracing::warn!(
            error = %e,
            "dropping expired credential records failed; they stay \
             listed until a later attempt"
        );
    }
    store.put(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_auth_bearer::Capability;

    #[test]
    fn an_issued_record_says_what_issued_it_and_when_it_ends() {
        let caps = CapabilitySet::new(vec![Capability::Read {
            scope: "auth".to_string(),
        }]);
        let record = issued_record(
            Issued {
                token_id: "t1",
                capabilities: &caps,
                issued_at_ms: 10_000,
                expires_at_ms: 10_000 + 86_400_000,
            },
            "kiosk",
            "kiosk-boot",
            CredentialSource::Kiosk,
            None,
        );
        assert_eq!(record.source, Some(CredentialSource::Kiosk));
        assert_eq!(record.expires_at_ms, Some(86_410_000));
        assert_eq!(
            record.expiry_policy,
            ExpiryPolicy::Seconds { value: 86_400 }
        );
        assert_eq!(record.scopes.len(), 1);
        assert_eq!(record.paired_device_id, None);
    }

    #[test]
    fn adding_a_record_drops_the_expired_ones() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        let caps = CapabilitySet::new(vec![]);
        let old = issued_record(
            Issued {
                token_id: "old",
                capabilities: &caps,
                issued_at_ms: 1_000,
                expires_at_ms: 2_000,
            },
            "old",
            "r",
            CredentialSource::Minted,
            None,
        );
        store.put(&old).unwrap();
        let new = issued_record(
            Issued {
                token_id: "new",
                capabilities: &caps,
                issued_at_ms: 5_000,
                expires_at_ms: 90_000,
            },
            "new",
            "r",
            CredentialSource::Minted,
            None,
        );
        add_record(&store, &new, 5_000).unwrap();
        let ids: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|r| r.token_id)
            .collect();
        assert_eq!(ids, ["new"]);
    }

    /// Adding records one after another scans the inventory once, not
    /// once per record: a record that lapses after the first addition
    /// is still there after the rest. A mint costs the same whatever
    /// the inventory holds.
    #[test]
    fn records_added_within_a_minute_scan_the_inventory_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        let caps = CapabilitySet::new(vec![]);
        let record = |id: &str, issued: u64, expires: u64| {
            issued_record(
                Issued {
                    token_id: id,
                    capabilities: &caps,
                    issued_at_ms: issued,
                    expires_at_ms: expires,
                },
                id,
                "r",
                CredentialSource::Minted,
                None,
            )
        };
        add_record(&store, &record("first", 5_000, 900_000), 5_000).unwrap();
        // Lapsed, and written after the first addition scanned.
        store.put(&record("lapsed", 1_000, 2_000)).unwrap();
        for i in 0..20 {
            add_record(
                &store,
                &record(&format!("next-{i}"), 5_000, 900_000),
                5_000,
            )
            .unwrap();
        }
        assert!(
            store.get("lapsed").is_ok(),
            "no addition after the first scanned the inventory"
        );
        assert_eq!(store.list().unwrap().len(), 22);
    }
}
