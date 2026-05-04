//! Release-tier trust roots.
//!
//! Plugin trust ([`KeyRole`](crate::KeyRole) and the `*.meta.toml`
//! sidecars under `/opt/evo/trust/`) authorises which keys may sign
//! plugin manifests. Release trust is the parallel layer for the
//! framework / device / vendor / commons release artefacts (the
//! `evo` steward binary, `evo-plugin-tool`, build-info manifests
//! published through the artefacts repository, signed plugin
//! bundles in their as-published shape). The two layers do not
//! share keys: a plugin's `manifest.sig` verifies against a
//! [`KeyRole`] key, a release artefact's `build-info.sig` verifies
//! against a [`ReleaseRole`] key.
//!
//! The four roles below are the layered chain
//! `PLUGIN_PACKAGING.md` §5 documents:
//!
//! - [`ReleaseRole::FrameworkRelease`] — `foonerd/evo-core`'s key,
//!   signs the steward binary, the operator CLI, and every
//!   artefact published through `foonerd/evo-core-artefacts`.
//! - [`ReleaseRole::ReferenceDeviceRelease`] — a reference
//!   generic device's key (`foonerd/evo-device-audio` for the
//!   audio domain), signs that device's images and the plugin
//!   bundles it hosts.
//! - [`ReleaseRole::VendorRelease`] — a vendor distribution's
//!   key, signs that vendor's images and vendor-published plugin
//!   bundles.
//! - [`ReleaseRole::CommonsRelease`] — the reference device's
//!   commons-signing identity, signs brand-neutral plugin bundles
//!   the reference device hosts on behalf of the framework's
//!   plugin commons. Distinct from [`ReleaseRole::ReferenceDeviceRelease`]
//!   only in `name_prefixes`: device-specific signs
//!   `org.evoframework.<domain>.*`, commons signs the broader
//!   `org.evoframework.*`.
//!
//! Verification protocol: a device fetches a release artefact,
//! reads the corresponding `build-info.toml`, verifies its
//! `build-info.sig` against a release-tier key whose role matches
//! the artefact's `kind` (per [`role_for_artefact_kind`]), and
//! refuses the install on signature, prefix, or trust-class
//! mismatch. The verifier walks the device's release-trust
//! directory layout (one sidecar `*.meta.toml` per public key,
//! same shape as the plugin-trust sidecar but with
//! `release_role` declared instead of `role`) and selects keys
//! whose `release_role` matches what the artefact's `kind`
//! requires.
//!
//! This module defines the role taxonomy and the `*.meta.toml`
//! parsing for release-tier sidecars; the actual signature
//! verification call sites remain caller-driven (the device
//! installer, the operator CLI's `verify-release` subcommand
//! when it lands, etc.) and use [`ed25519_dalek::VerifyingKey`]
//! directly with the documented `ed25519` raw-payload signing
//! shape.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;

use crate::error::TrustError;
use crate::key_meta::Authorisation;

/// Release-tier role taxonomy. Distinct from
/// [`crate::KeyRole`]: those keys sign plugin manifests; these
/// keys sign release artefacts published through an artefacts
/// repository.
///
/// Stored on disk in the per-key sidecar's `[key].release_role`
/// field, snake_case
/// (`framework_release` / `reference_device_release` /
/// `vendor_release` / `commons_release`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseRole {
    /// Framework release. Signs the `evo` steward binary,
    /// `evo-plugin-tool`, and every artefact published through
    /// `foonerd/evo-core-artefacts`.
    FrameworkRelease,
    /// Reference generic device release. Signs the device's
    /// images and the plugin bundles the device hosts under its
    /// own namespace (e.g. `org.evoframework.<domain>.*`).
    ReferenceDeviceRelease,
    /// Vendor distribution release. Signs vendor images and
    /// vendor-published plugin bundles under the vendor's own
    /// namespace.
    VendorRelease,
    /// Commons release. Signs brand-neutral plugin bundles a
    /// reference generic device hosts on behalf of the
    /// framework's plugin commons (the broader
    /// `org.evoframework.*` namespace).
    CommonsRelease,
}

impl ReleaseRole {
    /// Snake-case form of this role, stable on disk.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FrameworkRelease => "framework_release",
            Self::ReferenceDeviceRelease => "reference_device_release",
            Self::VendorRelease => "vendor_release",
            Self::CommonsRelease => "commons_release",
        }
    }
}

/// Translate a release-bundle `kind` string (as recorded in the
/// `build-info.toml` manifest's `kind` field per
/// `RELEASE_PLANE.md` §2.1) into the [`ReleaseRole`] whose key
/// is authorised to sign it.
///
/// The mapping reflects the convention `PLUGIN_PACKAGING.md` §5
/// documents:
///
/// - `core-binaries` — framework's own steward + tooling,
///   signed by [`ReleaseRole::FrameworkRelease`].
/// - `plugin-bundle` — defaults to
///   [`ReleaseRole::ReferenceDeviceRelease`] because the v0
///   release plane scopes plugin bundles to the reference
///   generic device. Vendor distributions publishing their own
///   plugin bundles override this binding by carrying their
///   own [`ReleaseRole::VendorRelease`] key in the device's
///   trust set; the verifier accepts whichever of the two
///   roles the bundle's manifest declares it was signed by
///   (the manifest carries an explicit `signed_by_role` field
///   when the default does not apply).
/// - `commons-bundle` — explicit commons-tier shape; signed
///   by [`ReleaseRole::CommonsRelease`].
///
/// Unknown kinds return `None`; callers refuse the install in
/// that case.
pub fn role_for_artefact_kind(kind: &str) -> Option<ReleaseRole> {
    Some(match kind {
        "core-binaries" => ReleaseRole::FrameworkRelease,
        "plugin-bundle" => ReleaseRole::ReferenceDeviceRelease,
        "commons-bundle" => ReleaseRole::CommonsRelease,
        _ => return None,
    })
}

/// Sidecar metadata for a release-tier public key.
///
/// Same on-disk shape as the plugin-trust [`crate::KeyMeta`]
/// (TOML, `[key]` + `[authorisation]` tables) with one diff:
/// the `[key].role` field is replaced by `[key].release_role`
/// carrying a [`ReleaseRole`] discriminant. The framework
/// rejects a release-tier sidecar whose `release_role` is
/// missing; defaulting it would silently bind a key to the
/// wrong tier.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseKeyMeta {
    /// `[key]` table.
    pub key: ReleaseKeySection,
    /// `[authorisation]` table — same vocabulary as plugin
    /// trust: `name_prefixes` and `max_trust_class`.
    pub authorisation: Authorisation,
}

/// `[key]` section of a release-tier sidecar.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseKeySection {
    /// Optional human-readable identifier for diagnostic logs.
    /// Distinct from the on-disk filename; sidecar files are
    /// keyed by filename and `key_id` only appears in errors.
    #[serde(default)]
    pub key_id: Option<String>,
    /// Algorithm whitelist marker. Only `"ed25519"` is accepted
    /// today; future expansion (post-quantum, Ed448) bumps
    /// this string and requires updating the verifier.
    #[serde(default)]
    pub algorithm: Option<String>,
    /// Hex-encoded blake3 digest of the public key bytes,
    /// used as a strong identifier for revocation lookups and
    /// chain-ancestry checks.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Mandatory release-role discriminant. The framework
    /// refuses sidecars without this field; defaulting it
    /// would silently bind a key to the wrong tier.
    pub release_role: ReleaseRole,
    /// Optional inclusive lower bound on validity. Verifier
    /// refuses signatures whose minted-at instant precedes
    /// this when set.
    #[serde(default)]
    pub not_before: Option<chrono::DateTime<chrono::Utc>>,
    /// Optional exclusive upper bound on validity.
    #[serde(default)]
    pub not_after: Option<chrono::DateTime<chrono::Utc>>,
}

/// One release-tier trust key plus its sidecar metadata.
///
/// Returned by the loader once per `<keyname>.pem` +
/// `<keyname>.meta.toml` pair found in the device's
/// release-trust directory.
#[derive(Debug, Clone)]
pub struct ReleaseTrustKey {
    /// Parsed `*.meta.toml` content.
    pub meta: ReleaseKeyMeta,
    /// The verifying half of the ed25519 keypair, parsed from
    /// the matching `<keyname>.pem`.
    pub verifying_key: VerifyingKey,
    /// The sidecar's filename stem (without extensions). Used
    /// for diagnostic messages and for matching against the
    /// optional `[key].key_id` field when set.
    pub key_id: String,
}

/// Verify a release-artefact signature against a set of
/// release-tier keys.
///
/// `payload` is the bytes that were signed (typically the
/// contents of a `build-info.toml`). `sig_bytes` is the raw
/// 64-byte ed25519 signature (as produced by
/// `openssl pkeyutl -sign -rawin`). `required_role` is the
/// role the verifier expects for this artefact kind (see
/// [`role_for_artefact_kind`]); a key with any other role is
/// refused even if its signature happens to verify.
///
/// On success, returns a reference to the matching
/// [`ReleaseTrustKey`] so the caller can record which key
/// admitted the artefact for audit logging. Returns
/// [`TrustError::MissingOrBadSignature`] when the supplied
/// bytes are not a 64-byte ed25519 signature, and
/// [`TrustError::SignatureNotRecognised`] when no key with
/// the required role verified the payload.
pub fn verify_release_signature<'a>(
    payload: &[u8],
    sig_bytes: &[u8],
    required_role: ReleaseRole,
    keys: &'a [ReleaseTrustKey],
) -> Result<&'a ReleaseTrustKey, TrustError> {
    // Per LOGGING.md §2: release-signature verification is a verb
    // invocation per release artefact admission.
    tracing::debug!(
        payload_bytes = payload.len(),
        sig_bytes = sig_bytes.len(),
        required_role = ?required_role,
        candidate_keys = keys.len(),
        "release trust verify: invoking"
    );
    if sig_bytes.len() != 64 {
        return Err(TrustError::MissingOrBadSignature);
    }
    let sig_array: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| TrustError::MissingOrBadSignature)?;
    let sig = Signature::from_bytes(&sig_array);
    for k in keys {
        if k.meta.key.release_role != required_role {
            continue;
        }
        if k.verifying_key.verify_strict(payload, &sig).is_ok() {
            tracing::debug!(
                key_id = ?k.key_id,
                "release trust verify: accepted"
            );
            return Ok(k);
        }
    }
    Err(TrustError::SignatureNotRecognised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_for_artefact_kind_maps_documented_kinds() {
        assert_eq!(
            role_for_artefact_kind("core-binaries"),
            Some(ReleaseRole::FrameworkRelease)
        );
        assert_eq!(
            role_for_artefact_kind("plugin-bundle"),
            Some(ReleaseRole::ReferenceDeviceRelease)
        );
        assert_eq!(
            role_for_artefact_kind("commons-bundle"),
            Some(ReleaseRole::CommonsRelease)
        );
        assert_eq!(role_for_artefact_kind("nonsense"), None);
    }

    #[test]
    fn release_role_str_round_trip_via_serde() {
        for (role, s) in [
            (ReleaseRole::FrameworkRelease, "framework_release"),
            (
                ReleaseRole::ReferenceDeviceRelease,
                "reference_device_release",
            ),
            (ReleaseRole::VendorRelease, "vendor_release"),
            (ReleaseRole::CommonsRelease, "commons_release"),
        ] {
            assert_eq!(role.as_str(), s);
            let toml_text = format!("release_role = \"{s}\"");
            #[derive(Deserialize)]
            struct W {
                release_role: ReleaseRole,
            }
            let parsed: W = toml::from_str(&toml_text).unwrap();
            assert_eq!(parsed.release_role, role);
        }
    }

    #[test]
    fn unknown_release_role_string_refuses() {
        let toml_text = "release_role = \"vendor\"";
        #[derive(Deserialize)]
        struct W {
            #[allow(dead_code)]
            release_role: ReleaseRole,
        }
        let r: Result<W, _> = toml::from_str(toml_text);
        assert!(r.is_err(), "non-release role string must be rejected");
    }

    #[test]
    fn release_key_section_requires_role() {
        // `[key]` table without release_role must fail to
        // parse. Defaulting it would silently bind a key to the
        // wrong tier.
        let toml_text = r#"
[key]
algorithm = "ed25519"

[authorisation]
name_prefixes = ["org.evoframework.core.*"]
max_trust_class = "platform"
"#;
        let r: Result<ReleaseKeyMeta, _> = toml::from_str(toml_text);
        assert!(r.is_err(), "missing release_role must be rejected");
    }

    #[test]
    fn release_key_section_parses_documented_shape() {
        let toml_text = r#"
[key]
key_id = "evo-core-release-2026"
algorithm = "ed25519"
fingerprint = "deadbeef"
release_role = "framework_release"

[authorisation]
name_prefixes = ["org.evoframework.core.*"]
max_trust_class = "platform"
"#;
        let m: ReleaseKeyMeta = toml::from_str(toml_text).unwrap();
        assert_eq!(m.key.release_role, ReleaseRole::FrameworkRelease);
        assert_eq!(m.key.key_id.as_deref(), Some("evo-core-release-2026"));
        assert_eq!(
            m.authorisation.name_prefixes,
            vec!["org.evoframework.core.*".to_string()]
        );
    }

    #[test]
    fn verify_release_signature_role_mismatch_refuses() {
        // A key whose release_role does not match the
        // required_role MUST be refused even if the
        // ed25519 signature happens to verify (it cannot here
        // because the keypair is unrelated to the payload, but
        // the role gate is the load-bearing check).
        use ed25519_dalek::SigningKey;
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key();
        let payload = b"build-info contents";
        let sig: ed25519_dalek::Signature =
            ed25519_dalek::Signer::sign(&sk, payload);
        let key = ReleaseTrustKey {
            meta: ReleaseKeyMeta {
                key: ReleaseKeySection {
                    key_id: Some("test".into()),
                    algorithm: Some("ed25519".into()),
                    fingerprint: None,
                    release_role: ReleaseRole::VendorRelease,
                    not_before: None,
                    not_after: None,
                },
                authorisation: Authorisation {
                    name_prefixes: vec!["com.test.*".into()],
                    max_trust_class:
                        evo_plugin_sdk::manifest::TrustClass::Standard,
                },
            },
            verifying_key: vk,
            key_id: "test".into(),
        };
        let r = verify_release_signature(
            payload,
            sig.to_bytes().as_ref(),
            ReleaseRole::FrameworkRelease,
            std::slice::from_ref(&key),
        );
        assert!(matches!(r, Err(TrustError::SignatureNotRecognised)));
    }

    #[test]
    fn verify_release_signature_correct_role_admits() {
        use ed25519_dalek::SigningKey;
        let sk = SigningKey::from_bytes(&[11u8; 32]);
        let vk = sk.verifying_key();
        let payload = b"core-binaries manifest payload";
        let sig: ed25519_dalek::Signature =
            ed25519_dalek::Signer::sign(&sk, payload);
        let key = ReleaseTrustKey {
            meta: ReleaseKeyMeta {
                key: ReleaseKeySection {
                    key_id: Some("evo-core".into()),
                    algorithm: Some("ed25519".into()),
                    fingerprint: None,
                    release_role: ReleaseRole::FrameworkRelease,
                    not_before: None,
                    not_after: None,
                },
                authorisation: Authorisation {
                    name_prefixes: vec!["org.evoframework.core.*".into()],
                    max_trust_class:
                        evo_plugin_sdk::manifest::TrustClass::Platform,
                },
            },
            verifying_key: vk,
            key_id: "evo-core".into(),
        };
        let admitted = verify_release_signature(
            payload,
            sig.to_bytes().as_ref(),
            ReleaseRole::FrameworkRelease,
            std::slice::from_ref(&key),
        )
        .expect("framework key admits framework artefact");
        assert_eq!(admitted.key_id, "evo-core");
    }

    #[test]
    fn verify_release_signature_wrong_length_refuses() {
        let key = ReleaseTrustKey {
            meta: ReleaseKeyMeta {
                key: ReleaseKeySection {
                    key_id: None,
                    algorithm: Some("ed25519".into()),
                    fingerprint: None,
                    release_role: ReleaseRole::FrameworkRelease,
                    not_before: None,
                    not_after: None,
                },
                authorisation: Authorisation {
                    name_prefixes: vec!["x.*".into()],
                    max_trust_class:
                        evo_plugin_sdk::manifest::TrustClass::Standard,
                },
            },
            verifying_key: ed25519_dalek::SigningKey::from_bytes(&[1u8; 32])
                .verifying_key(),
            key_id: "x".into(),
        };
        let r = verify_release_signature(
            b"payload",
            &[0u8; 32],
            ReleaseRole::FrameworkRelease,
            std::slice::from_ref(&key),
        );
        assert!(matches!(r, Err(TrustError::MissingOrBadSignature)));
    }
}
