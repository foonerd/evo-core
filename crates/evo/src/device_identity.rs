// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Persistent device identity — multi-room foundation.
//!
//! Each evo device has a stable [`DeviceIdentity`] generated
//! at first boot and durable across reinstall. The framework
//! owns the canonical id + display name + creation timestamp;
//! vendor distributions optionally populate the public-key
//! slot through the pluggable cryptographic-services hook
//! the framework exposes (per-device cryptographic identity
//! is vendor-distribution scope, not framework scope).
//!
//! Subsequent multi-room sub-primitives consume this identity:
//!
//! - mDNS-SD discovery payloads embed the device id +
//!   display name + public-key fingerprint + capability flags.
//! - Group entity records reference member device ids.
//! - Source-host election operates over device ids.
//! - Action-ledger entries name the device by its id.
//!
//! The identity is generated exactly once: at first boot when
//! no row exists in the substrate. Subsequent boots read the
//! existing row. Rename is a separate write path; rotation of
//! the canonical id is intentionally not provided — the id is
//! contractually stable across reinstall (the substrate row
//! survives database files; even on database loss the
//! operator restores from backup rather than minting a new
//! id, which would orphan group memberships and pairing
//! ledgers).

use std::sync::Arc;

use evo_primitives::{
    DeviceId, DeviceIdentity, NameSource, DISPLAY_NAME_MAX_LEN,
};

use crate::persistence::{
    PersistedDeviceIdentity, PersistenceError, PersistenceStore,
};

// The canonical `DeviceId` type lives in the foundation crate
// `evo-primitives`. Consumers within this crate import directly
// from `evo_primitives::DeviceId`.

// `NameSource`, `DeviceIdentity`, and `DISPLAY_NAME_MAX_LEN`
// live in the foundation crate `evo-primitives`. They are
// imported above and used here verbatim; consumers within this
// crate continue to reach them through the `crate::device_identity`
// path via the `pub use` re-export style is intentionally
// avoided in favour of direct `evo_primitives` imports at each
// call site.

/// Sanity-check an OS hostname before adopting it as the
/// first-boot `display_name` seed. Rejects empty strings,
/// whitespace-only strings, the conventional placeholder
/// "localhost" / "(none)", any string longer than
/// [`DISPLAY_NAME_MAX_LEN`] bytes, and any string containing
/// ASCII control characters. Returns the trimmed sane form on
/// success.
fn sane_hostname_seed(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Common placeholder values surfaced by systemd-hostnamed
    // / Debian first-boot images / containers.
    let lc = trimmed.to_ascii_lowercase();
    if matches!(lc.as_str(), "localhost" | "(none)" | "raspberrypi") {
        return None;
    }
    if trimmed.len() > DISPLAY_NAME_MAX_LEN {
        return None;
    }
    if trimmed.chars().any(|c| c.is_ascii_control()) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Read the OS hostname via the `gethostname` crate and apply
/// [`sane_hostname_seed`]. Returns `None` when the platform
/// returns no hostname or the value fails sanity checks.
fn read_seed_hostname() -> Option<String> {
    let os_hostname = gethostname::gethostname();
    let s = os_hostname.to_string_lossy();
    sane_hostname_seed(&s)
}

impl From<PersistedDeviceIdentity> for DeviceIdentity {
    fn from(p: PersistedDeviceIdentity) -> Self {
        Self {
            device_id: DeviceId(p.device_id),
            display_name: p.display_name,
            vendor_id: p.vendor_id,
            public_key_bytes: p.public_key_bytes,
            created_at_ms: p.created_at_ms,
            name_source: p.name_source,
        }
    }
}

impl From<&DeviceIdentity> for PersistedDeviceIdentity {
    fn from(d: &DeviceIdentity) -> Self {
        Self {
            device_id: d.device_id.0.clone(),
            display_name: d.display_name.clone(),
            vendor_id: d.vendor_id.clone(),
            public_key_bytes: d.public_key_bytes.clone(),
            created_at_ms: d.created_at_ms,
            name_source: d.name_source,
        }
    }
}

/// Errors raised by [`DeviceIdentityStore`].
#[derive(Debug, thiserror::Error)]
pub enum DeviceIdentityError {
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Operator submitted an invalid display name (empty or
    /// excessively long).
    #[error("display_name validation: {0}")]
    InvalidDisplayName(String),
}

/// Persistence-backed accessor for the singleton device
/// identity. Constructed once at boot; the boot path calls
/// [`Self::ensure`] which generates the identity at first
/// boot or returns the existing row on subsequent boots.
#[derive(Debug, Clone)]
pub struct DeviceIdentityStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl DeviceIdentityStore {
    /// Construct a store wrapping the supplied persistence
    /// handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Ensure a device identity exists. First-boot path:
    /// generates a fresh `DeviceId` (UUIDv4); seeds
    /// `display_name` from the OS hostname (when sane —
    /// non-empty, length ≤ 63, no control chars, not a
    /// placeholder like `localhost`); falls back to
    /// `evo-<short>` when the platform yields no usable
    /// hostname; sets `name_source = Auto`; persists via
    /// `put_device_identity`. Subsequent boots load the
    /// existing row unchanged. The supplied `vendor_id` is
    /// recorded only on first-boot creation; subsequent
    /// boots inherit the previously-recorded vendor_id (the
    /// substrate is the source of truth across reinstall).
    pub async fn ensure(
        &self,
        vendor_id: Option<&str>,
    ) -> Result<DeviceIdentity, DeviceIdentityError> {
        if let Some(existing) = self.get().await? {
            return Ok(existing);
        }
        let device_id = DeviceId::generate();
        let display_name = read_seed_hostname()
            .unwrap_or_else(|| format!("evo-{}", device_id.short()));
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let identity = DeviceIdentity {
            device_id,
            display_name,
            vendor_id: vendor_id.map(|s| s.to_string()),
            public_key_bytes: None,
            created_at_ms: now_ms,
            name_source: NameSource::Auto,
        };
        self.persistence
            .put_device_identity(PersistedDeviceIdentity::from(&identity))
            .await?;
        Ok(identity)
    }

    /// Read the device identity. Returns `None` before
    /// first-boot generation.
    pub async fn get(
        &self,
    ) -> Result<Option<DeviceIdentity>, DeviceIdentityError> {
        let row = self.persistence.get_device_identity().await?;
        Ok(row.map(DeviceIdentity::from))
    }

    /// Update the operator-editable display name. Refuses
    /// empty / whitespace-only names and names exceeding 128
    /// characters. Promotes [`DeviceIdentity::name_source`] to
    /// [`NameSource::Operator`] on success — the collision
    /// resolver MUST NOT rewrite the name thereafter.
    pub async fn set_display_name(
        &self,
        new_name: &str,
    ) -> Result<DeviceIdentity, DeviceIdentityError> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err(DeviceIdentityError::InvalidDisplayName(
                "display_name must not be empty or whitespace-only".into(),
            ));
        }
        if trimmed.chars().count() > 128 {
            return Err(DeviceIdentityError::InvalidDisplayName(format!(
                "display_name must be \u{2264} 128 chars (got {})",
                trimmed.chars().count()
            )));
        }
        let mut identity = self.get().await?.ok_or_else(|| {
            DeviceIdentityError::InvalidDisplayName(
                "no device identity recorded yet — boot must call \
                     ensure() first"
                    .into(),
            )
        })?;
        identity.display_name = trimmed.to_string();
        identity.name_source = NameSource::Operator;
        self.persistence
            .put_device_identity(PersistedDeviceIdentity::from(&identity))
            .await?;
        Ok(identity)
    }

    /// Reset the operator-editable display name to its default,
    /// re-seeded from the OS hostname when sane (falls back to
    /// `evo-<short>` otherwise), and set
    /// [`DeviceIdentity::name_source`] back to
    /// [`NameSource::Auto`]. Used when the operator wants to
    /// undo a prior rename or re-derive the name after an OS
    /// hostname change.
    ///
    /// Returns the resolved identity. The caller is responsible
    /// for re-advertising the mDNS-SD payload + emitting the
    /// `device_display_name_changed` happening.
    pub async fn reset_display_name_to_default(
        &self,
    ) -> Result<DeviceIdentity, DeviceIdentityError> {
        let mut identity = self.get().await?.ok_or_else(|| {
            DeviceIdentityError::InvalidDisplayName(
                "no device identity recorded yet — boot must call \
                     ensure() first"
                    .into(),
            )
        })?;
        let new_default = read_seed_hostname()
            .unwrap_or_else(|| format!("evo-{}", identity.device_id.short()));
        identity.display_name = new_default;
        identity.name_source = NameSource::Auto;
        self.persistence
            .put_device_identity(PersistedDeviceIdentity::from(&identity))
            .await?;
        Ok(identity)
    }

    /// Apply the collision resolver. When the local identity
    /// is `Auto`-sourced AND the current `display_name` does
    /// NOT already carry the device-id suffix, rewrite it to
    /// `<current_display_name>-<id-short>` and persist. The
    /// resolver is a no-op when:
    ///
    /// - [`DeviceIdentity::name_source`] is [`NameSource::Operator`]
    ///   (sticky operator choice, never rewritten); or
    /// - the current `display_name` already ends in the
    ///   `-<id-short>` suffix (idempotent — repeated collision
    ///   events do not double-suffix).
    ///
    /// Returns the resolved identity. The caller is
    /// responsible for re-advertising the mDNS-SD payload
    /// when the display_name actually changed; the boolean in
    /// the returned tuple signals whether a rewrite occurred.
    pub async fn resolve_collision(
        &self,
    ) -> Result<(DeviceIdentity, bool), DeviceIdentityError> {
        let mut identity = self.get().await?.ok_or_else(|| {
            DeviceIdentityError::InvalidDisplayName(
                "no device identity recorded yet — boot must call \
                     ensure() first"
                    .into(),
            )
        })?;
        if identity.name_source == NameSource::Operator {
            return Ok((identity, false));
        }
        let suffix = format!("-{}", identity.device_id.short());
        if identity.display_name.ends_with(&suffix) {
            return Ok((identity, false));
        }
        identity.display_name = format!("{}{}", identity.display_name, suffix);
        self.persistence
            .put_device_identity(PersistedDeviceIdentity::from(&identity))
            .await?;
        Ok((identity, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn store() -> DeviceIdentityStore {
        DeviceIdentityStore::new(Arc::new(MemoryPersistenceStore::default()))
    }

    #[test]
    fn device_id_generates_uuid_form() {
        let id = DeviceId::generate();
        // UUIDv4 string form: 36 chars with hyphens at fixed
        // positions.
        assert_eq!(id.0.len(), 36);
        assert_eq!(id.0.as_bytes()[8], b'-');
        assert_eq!(id.0.as_bytes()[13], b'-');
        assert_eq!(id.0.as_bytes()[18], b'-');
        assert_eq!(id.0.as_bytes()[23], b'-');
    }

    #[test]
    fn device_id_short_takes_first_eight_hex_chars() {
        let id = DeviceId("550e8400-e29b-41d4-a716-446655440000".into());
        assert_eq!(id.short(), "550e8400");
    }

    #[tokio::test]
    async fn ensure_generates_identity_on_first_boot() {
        let s = store();
        let id1 = s.ensure(Some("test-vendor")).await.unwrap();
        assert_eq!(id1.vendor_id.as_deref(), Some("test-vendor"));
        // Display name is either the OS hostname (when sane) or
        // the `evo-<short>` fallback; both shapes are valid Auto
        // seeds. The non-emptiness + Auto provenance is the
        // invariant we care about at first-boot generation.
        assert!(!id1.display_name.is_empty());
        assert_eq!(id1.name_source, NameSource::Auto);
        assert_eq!(id1.public_key_bytes, None);
        assert!(id1.created_at_ms > 0);
    }

    #[tokio::test]
    async fn ensure_falls_back_to_evo_short_when_hostname_unusable() {
        // We cannot easily mock gethostname in a unit test, but
        // we can assert that whatever shape `ensure` chose is
        // either the host's sane hostname OR the
        // `evo-<short>` fallback — never an empty / placeholder
        // string.
        let s = store();
        let id = s.ensure(None).await.unwrap();
        let short = id.device_id.short();
        let evo_form = format!("evo-{}", short);
        let is_evo_fallback = id.display_name == evo_form;
        let is_sane_hostname = sane_hostname_seed(&id.display_name).is_some();
        assert!(
            is_evo_fallback || is_sane_hostname,
            "display_name {:?} must be either `evo-<short>` or a sane hostname",
            id.display_name
        );
    }

    #[tokio::test]
    async fn ensure_is_idempotent_on_subsequent_calls() {
        let s = store();
        let id1 = s.ensure(Some("first-vendor")).await.unwrap();
        // Subsequent ensure with a different vendor_id does
        // NOT re-generate or change the existing identity.
        let id2 = s.ensure(Some("different-vendor")).await.unwrap();
        assert_eq!(id1, id2);
        assert_eq!(id2.vendor_id.as_deref(), Some("first-vendor"));
    }

    #[tokio::test]
    async fn get_before_ensure_returns_none() {
        let s = store();
        let got = s.get().await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn set_display_name_rejects_empty_string() {
        let s = store();
        s.ensure(None).await.unwrap();
        let err = s
            .set_display_name("")
            .await
            .expect_err("empty name must be refused");
        assert!(matches!(err, DeviceIdentityError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn set_display_name_rejects_whitespace_only() {
        let s = store();
        s.ensure(None).await.unwrap();
        let err = s
            .set_display_name("   \t\n  ")
            .await
            .expect_err("whitespace-only name must be refused");
        assert!(matches!(err, DeviceIdentityError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn set_display_name_rejects_excessive_length() {
        let s = store();
        s.ensure(None).await.unwrap();
        let too_long: String = "x".repeat(129);
        let err = s
            .set_display_name(&too_long)
            .await
            .expect_err("name > 128 chars must be refused");
        assert!(matches!(err, DeviceIdentityError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn set_display_name_trims_whitespace() {
        let s = store();
        s.ensure(None).await.unwrap();
        let id = s.set_display_name("  Listening Room  ").await.unwrap();
        assert_eq!(id.display_name, "Listening Room");
    }

    #[tokio::test]
    async fn set_display_name_persists_change() {
        let s = store();
        s.ensure(None).await.unwrap();
        s.set_display_name("Kitchen").await.unwrap();
        let got = s.get().await.unwrap().unwrap();
        assert_eq!(got.display_name, "Kitchen");
    }

    #[tokio::test]
    async fn set_display_name_before_ensure_refuses() {
        let s = store();
        let err = s
            .set_display_name("Kitchen")
            .await
            .expect_err("set before ensure must refuse");
        assert!(matches!(err, DeviceIdentityError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn device_id_is_stable_across_set_display_name() {
        let s = store();
        let id1 = s.ensure(None).await.unwrap();
        s.set_display_name("Renamed").await.unwrap();
        let id2 = s.get().await.unwrap().unwrap();
        assert_eq!(id1.device_id, id2.device_id);
        assert_eq!(id1.created_at_ms, id2.created_at_ms);
    }

    #[test]
    fn identity_round_trips_through_serde() {
        let id = DeviceIdentity {
            device_id: DeviceId("550e8400-e29b-41d4-a716-446655440000".into()),
            display_name: "Listening Room".into(),
            vendor_id: Some("audiophile-os".into()),
            public_key_bytes: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            created_at_ms: 1_700_000_000_000,
            name_source: NameSource::Operator,
        };
        let json = serde_json::to_string(&id).unwrap();
        let parsed: DeviceIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn sane_hostname_seed_rejects_empty_and_placeholders() {
        assert_eq!(sane_hostname_seed(""), None);
        assert_eq!(sane_hostname_seed("   "), None);
        assert_eq!(sane_hostname_seed("localhost"), None);
        assert_eq!(sane_hostname_seed("LocalHost"), None);
        assert_eq!(sane_hostname_seed("(none)"), None);
        assert_eq!(sane_hostname_seed("raspberrypi"), None);
        let too_long: String = "a".repeat(64);
        assert_eq!(sane_hostname_seed(&too_long), None);
        assert_eq!(sane_hostname_seed("bed\nroom"), None);
    }

    #[test]
    fn sane_hostname_seed_accepts_room_like_names() {
        assert_eq!(sane_hostname_seed("bedroom").as_deref(), Some("bedroom"));
        assert_eq!(
            sane_hostname_seed("  kitchen  ").as_deref(),
            Some("kitchen")
        );
        assert_eq!(
            sane_hostname_seed("listening-room").as_deref(),
            Some("listening-room")
        );
    }

    #[tokio::test]
    async fn identity_serde_defaults_name_source_to_auto_for_legacy_rows() {
        // Legacy rows have no `name_source` field. The
        // serde default must rehydrate them as `Auto` so the
        // collision resolver remains free to rewrite them.
        let legacy_json = r#"{
            "device_id": "550e8400-e29b-41d4-a716-446655440000",
            "display_name": "evo-550e8400",
            "created_at_ms": 1700000000000
        }"#;
        let parsed: DeviceIdentity = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.name_source, NameSource::Auto);
    }

    #[tokio::test]
    async fn set_display_name_promotes_name_source_to_operator() {
        let s = store();
        let id1 = s.ensure(None).await.unwrap();
        assert_eq!(id1.name_source, NameSource::Auto);
        let id2 = s.set_display_name("Kitchen").await.unwrap();
        assert_eq!(id2.name_source, NameSource::Operator);
    }

    #[tokio::test]
    async fn resolve_collision_rewrites_auto_name_with_id_suffix() {
        let s = store();
        // Seed an Auto identity with a stable display_name so
        // we can predict the rewrite shape independently of the
        // host the test runs on.
        let initial = DeviceIdentity {
            device_id: DeviceId("550e8400-e29b-41d4-a716-446655440000".into()),
            display_name: "bedroom".into(),
            vendor_id: None,
            public_key_bytes: None,
            created_at_ms: 1,
            name_source: NameSource::Auto,
        };
        s.persistence
            .put_device_identity(PersistedDeviceIdentity::from(&initial))
            .await
            .unwrap();

        let (resolved, changed) = s.resolve_collision().await.unwrap();
        assert!(changed);
        assert_eq!(resolved.display_name, "bedroom-550e8400");
        // Idempotent on second invocation — already suffixed.
        let (again, changed_again) = s.resolve_collision().await.unwrap();
        assert!(!changed_again);
        assert_eq!(again.display_name, "bedroom-550e8400");
    }

    #[tokio::test]
    async fn resolve_collision_is_noop_for_operator_sourced_names() {
        let s = store();
        let initial = DeviceIdentity {
            device_id: DeviceId("550e8400-e29b-41d4-a716-446655440000".into()),
            display_name: "Bedroom".into(),
            vendor_id: None,
            public_key_bytes: None,
            created_at_ms: 1,
            name_source: NameSource::Operator,
        };
        s.persistence
            .put_device_identity(PersistedDeviceIdentity::from(&initial))
            .await
            .unwrap();

        let (after, changed) = s.resolve_collision().await.unwrap();
        assert!(!changed);
        assert_eq!(after.display_name, "Bedroom");
        assert_eq!(after.name_source, NameSource::Operator);
    }

    #[tokio::test]
    async fn resolve_collision_is_noop_for_fallback_evo_short_form() {
        // `evo-<short>` already ends in `-<id-short>`; resolver
        // must treat it as already-disambiguated.
        let s = store();
        let initial = DeviceIdentity {
            device_id: DeviceId("550e8400-e29b-41d4-a716-446655440000".into()),
            display_name: "evo-550e8400".into(),
            vendor_id: None,
            public_key_bytes: None,
            created_at_ms: 1,
            name_source: NameSource::Auto,
        };
        s.persistence
            .put_device_identity(PersistedDeviceIdentity::from(&initial))
            .await
            .unwrap();

        let (after, changed) = s.resolve_collision().await.unwrap();
        assert!(!changed);
        assert_eq!(after.display_name, "evo-550e8400");
    }
}
