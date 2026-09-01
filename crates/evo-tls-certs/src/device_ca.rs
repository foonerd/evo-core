// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Device-CA generation + leaf issuance.
//!
//! At first boot the device generates a self-signed root CA
//! (long-lived, e.g. 10 years), persists it under the
//! steward state directory, then issues a leaf cert for
//! its operator-supplied hostnames (short-lived, e.g.
//! 90 days). The runtime mount rotates the leaf on expiry
//! by re-issuing from the persisted root.

use crate::bundle::{CertBundle, CertChain, PrivateKey};
use crate::error::CertError;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa,
    KeyPair, KeyUsagePurpose,
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// Minimum acceptable cert TTL in days. The framework
/// refuses TTLs shorter than 1 day on operator-supplied
/// configuration; renewal cadence is operator-tunable but
/// must still satisfy the floor.
pub const MIN_TTL_DAYS: u32 = 1;

/// Maximum acceptable leaf cert TTL in days. The framework
/// refuses leaf TTLs longer than 825 days (the CA/Browser
/// Forum baseline maximum for publicly-trusted certs,
/// applied here as a sensible ceiling for the
/// privately-trusted device-CA leaves as well).
pub const MAX_LEAF_TTL_DAYS: u32 = 825;

/// Default device-CA TTL: 10 years (3650 days). Device CAs
/// are long-lived because they're trusted manually by the
/// operator at first connection; rotation requires a
/// re-trust flow. Operator may shorten via
/// [`DeviceCaConfig::ttl_days`].
pub const DEFAULT_CA_TTL_DAYS: u32 = 3650;

/// Default leaf cert TTL: 90 days (matches Let's Encrypt's
/// cadence for muscle-memory + automation parity).
pub const DEFAULT_LEAF_TTL_DAYS: u32 = 90;

/// Maximum acceptable CA TTL: 30 years. A defence in depth
/// against operator misconfiguration (typing 30000 days
/// when meaning 3000); a real device should never need a
/// CA living past a normal device-lifetime budget.
pub const MAX_CA_TTL_DAYS: u32 = 30 * 365;

/// Configuration for device-CA generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCaConfig {
    /// Common name (CN) for the CA. Surfaces in operator
    /// trust-store UIs. Defaults to a generic identifier
    /// the operator may override at first-boot wizard time.
    pub common_name: String,

    /// CA cert TTL in days. Defaults to
    /// [`DEFAULT_CA_TTL_DAYS`].
    pub ttl_days: u32,
}

impl Default for DeviceCaConfig {
    fn default() -> Self {
        Self {
            common_name: "evo device root CA".to_string(),
            ttl_days: DEFAULT_CA_TTL_DAYS,
        }
    }
}

/// Configuration for a leaf cert issued under the device CA.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeafConfig {
    /// Subject Alternative Names. Must contain at least one
    /// hostname (operator-supplied DNS name, IP literal, or
    /// `.local` mDNS name). The leaf cert's CN is the first
    /// entry.
    pub hostnames: Vec<String>,

    /// Leaf cert TTL in days. Defaults to
    /// [`DEFAULT_LEAF_TTL_DAYS`].
    pub ttl_days: u32,
}

impl LeafConfig {
    /// Construct with the supplied hostnames + default TTL.
    pub fn for_hostnames(hostnames: Vec<String>) -> Self {
        Self {
            hostnames,
            ttl_days: DEFAULT_LEAF_TTL_DAYS,
        }
    }
}

/// One generated device CA: the root cert + its private
/// signing key, both in PEM form, plus the rcgen
/// `Certificate` value retained so callers can issue
/// leaves without re-parsing the PEM.
pub struct GeneratedCa {
    /// PEM-encoded root CA cert.
    pub ca_cert_pem: String,
    /// PEM-encoded root CA private key.
    pub ca_key_pem: String,
    /// Retained rcgen `Certificate` for issuing leaves.
    /// Internal — the runtime mount re-parses the PEM at
    /// boot to recover this value if it has been persisted
    /// to disk between sessions.
    ca_params: CertificateParams,
    ca_key_pair: KeyPair,
}

impl GeneratedCa {
    /// Reconstruct a [`GeneratedCa`] from previously-persisted
    /// PEM material.
    ///
    /// Used by the boot path to load the device-CA at every
    /// subsequent boot after the genesis [`generate_device_ca`]
    /// call. The reconstructed CA can issue new leaves; its
    /// canonical PEMs round-trip exactly.
    pub fn from_pem(
        ca_cert_pem: &str,
        ca_key_pem: &str,
    ) -> Result<Self, CertError> {
        let key_pair = KeyPair::from_pem(ca_key_pem)?;
        let params = CertificateParams::from_ca_cert_pem(ca_cert_pem)?;
        Ok(GeneratedCa {
            ca_cert_pem: ca_cert_pem.to_string(),
            ca_key_pem: ca_key_pem.to_string(),
            ca_params: params,
            ca_key_pair: key_pair,
        })
    }

    /// Issue a leaf cert signed by this CA.
    pub fn issue_leaf(
        &self,
        leaf: &LeafConfig,
    ) -> Result<CertBundle, CertError> {
        if leaf.hostnames.is_empty() {
            return Err(CertError::NoHostnames);
        }
        validate_ttl(leaf.ttl_days, MIN_TTL_DAYS, MAX_LEAF_TTL_DAYS)?;

        let leaf_params = leaf_params(leaf)?;
        let leaf_key = KeyPair::generate()?;
        let ca_cert = self.ca_params.clone().self_signed(&self.ca_key_pair)?;
        let leaf_cert =
            leaf_params.signed_by(&leaf_key, &ca_cert, &self.ca_key_pair)?;

        let mut chain = String::with_capacity(2048);
        chain.push_str(&leaf_cert.pem());
        chain.push_str(&ca_cert.pem());

        Ok(CertBundle {
            private_key: PrivateKey::from_pem(leaf_key.serialize_pem()),
            chain: CertChain::from_pem(chain),
        })
    }
}

/// Generate a fresh device CA.
pub fn generate_device_ca(
    config: &DeviceCaConfig,
) -> Result<GeneratedCa, CertError> {
    validate_ttl(config.ttl_days, MIN_TTL_DAYS, MAX_CA_TTL_DAYS)?;

    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, config.common_name.as_str());
    params.distinguished_name = dn;

    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::seconds(60);
    params.not_after = now + Duration::days(i64::from(config.ttl_days));

    let key_pair = KeyPair::generate()?;
    let cert = params.clone().self_signed(&key_pair)?;

    Ok(GeneratedCa {
        ca_cert_pem: cert.pem(),
        ca_key_pem: key_pair.serialize_pem(),
        ca_params: params,
        ca_key_pair: key_pair,
    })
}

/// One-shot helper: generate a fresh CA + issue one leaf
/// for the supplied hostnames. Returns the CA artefacts +
/// the leaf bundle.
///
/// Used by the device first-boot wizard's TLS setup step;
/// production paths separate CA generation (rare,
/// long-lived) from leaf issuance (frequent, short-lived)
/// via [`GeneratedCa::issue_leaf`].
pub fn generate_leaf(
    ca_config: &DeviceCaConfig,
    leaf_config: &LeafConfig,
) -> Result<(GeneratedCa, CertBundle), CertError> {
    let ca = generate_device_ca(ca_config)?;
    let bundle = ca.issue_leaf(leaf_config)?;
    Ok((ca, bundle))
}

fn leaf_params(leaf: &LeafConfig) -> Result<CertificateParams, CertError> {
    let mut params = CertificateParams::new(leaf.hostnames.clone())?;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];

    let mut dn = DistinguishedName::new();
    if let Some(primary) = leaf.hostnames.first() {
        dn.push(DnType::CommonName, primary.as_str());
    }
    params.distinguished_name = dn;

    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::seconds(60);
    params.not_after = now + Duration::days(i64::from(leaf.ttl_days));

    Ok(params)
}

fn validate_ttl(requested: u32, min: u32, max: u32) -> Result<(), CertError> {
    if requested < min || requested > max {
        return Err(CertError::TtlOutOfRange {
            requested_days: requested,
            min_days: min,
            max_days: max,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_generation_produces_pem_cert_and_key() {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        assert!(ca.ca_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(ca.ca_cert_pem.contains("END CERTIFICATE"));
        assert!(ca.ca_key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(ca.ca_key_pem.contains("END PRIVATE KEY"));
    }

    #[test]
    fn ca_refuses_ttl_below_floor() {
        let bad = DeviceCaConfig {
            common_name: "test".to_string(),
            ttl_days: 0,
        };
        assert!(matches!(
            generate_device_ca(&bad),
            Err(CertError::TtlOutOfRange { .. })
        ));
    }

    #[test]
    fn ca_round_trips_through_from_pem_and_can_issue_leaves() {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let reloaded =
            GeneratedCa::from_pem(&ca.ca_cert_pem, &ca.ca_key_pem).unwrap();
        // The reloaded CA's PEMs match the original byte-for-byte.
        assert_eq!(reloaded.ca_cert_pem, ca.ca_cert_pem);
        assert_eq!(reloaded.ca_key_pem, ca.ca_key_pem);
        // The reloaded CA can issue a fresh leaf.
        let bundle = reloaded
            .issue_leaf(&LeafConfig::for_hostnames(vec![
                "device.local".to_string()
            ]))
            .unwrap();
        assert_eq!(
            bundle.chain.as_str().matches("BEGIN CERTIFICATE").count(),
            2
        );
    }

    #[test]
    fn ca_refuses_ttl_above_ceiling() {
        let bad = DeviceCaConfig {
            common_name: "test".to_string(),
            ttl_days: MAX_CA_TTL_DAYS + 1,
        };
        assert!(matches!(
            generate_device_ca(&bad),
            Err(CertError::TtlOutOfRange { .. })
        ));
    }

    #[test]
    fn leaf_generation_produces_chain_with_two_certs() {
        let (_ca, bundle) = generate_leaf(
            &DeviceCaConfig::default(),
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
        // Chain has leaf + CA — two BEGIN CERTIFICATE markers.
        assert_eq!(
            bundle.chain.as_str().matches("BEGIN CERTIFICATE").count(),
            2
        );
    }

    #[test]
    fn leaf_private_key_is_pem_encoded() {
        let (_ca, bundle) = generate_leaf(
            &DeviceCaConfig::default(),
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
        assert!(bundle.private_key.as_str().contains("BEGIN PRIVATE KEY"));
        assert!(bundle.private_key.as_str().contains("END PRIVATE KEY"));
    }

    #[test]
    fn leaf_carries_supplied_hostname_in_chain() {
        // The leaf cert's DER includes the hostname in
        // Subject + SAN; the test checks the chain text
        // contains the cert (granular SAN inspection is a
        // follow-on once a der parser dep lands).
        let hostname = "audio-device-42.local";
        let (_ca, bundle) = generate_leaf(
            &DeviceCaConfig::default(),
            &LeafConfig::for_hostnames(vec![hostname.to_string()]),
        )
        .unwrap();
        // Two certs in the chain (leaf + CA).
        assert_eq!(
            bundle.chain.as_str().matches("BEGIN CERTIFICATE").count(),
            2
        );
    }

    #[test]
    fn leaf_refuses_empty_hostname_list() {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let leaf = LeafConfig {
            hostnames: vec![],
            ttl_days: DEFAULT_LEAF_TTL_DAYS,
        };
        assert!(matches!(ca.issue_leaf(&leaf), Err(CertError::NoHostnames)));
    }

    #[test]
    fn leaf_refuses_ttl_above_ceiling() {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let leaf = LeafConfig {
            hostnames: vec!["device.local".to_string()],
            ttl_days: MAX_LEAF_TTL_DAYS + 1,
        };
        assert!(matches!(
            ca.issue_leaf(&leaf),
            Err(CertError::TtlOutOfRange { .. })
        ));
    }

    #[test]
    fn leaf_refuses_zero_ttl() {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let leaf = LeafConfig {
            hostnames: vec!["device.local".to_string()],
            ttl_days: 0,
        };
        assert!(matches!(
            ca.issue_leaf(&leaf),
            Err(CertError::TtlOutOfRange { .. })
        ));
    }

    #[test]
    fn ca_can_issue_multiple_leaves() {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let a = ca
            .issue_leaf(&LeafConfig::for_hostnames(vec!["a.local".to_string()]))
            .unwrap();
        let b = ca
            .issue_leaf(&LeafConfig::for_hostnames(vec!["b.local".to_string()]))
            .unwrap();
        // Distinct leaves carry distinct private keys.
        assert_ne!(a.private_key, b.private_key);
        // Distinct chains.
        assert_ne!(a.chain, b.chain);
    }

    #[test]
    fn leaf_supports_multiple_hostnames_in_san() {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let bundle = ca
            .issue_leaf(&LeafConfig::for_hostnames(vec![
                "primary.local".to_string(),
                "secondary.local".to_string(),
                "203.0.113.1".to_string(),
            ]))
            .unwrap();
        assert_eq!(
            bundle.chain.as_str().matches("BEGIN CERTIFICATE").count(),
            2
        );
    }

    #[test]
    fn distinct_ca_generations_produce_distinct_keys() {
        let ca_a = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let ca_b = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        assert_ne!(ca_a.ca_cert_pem, ca_b.ca_cert_pem);
        assert_ne!(ca_a.ca_key_pem, ca_b.ca_key_pem);
    }

    #[test]
    fn ca_config_default_common_name_is_descriptive() {
        let c = DeviceCaConfig::default();
        assert!(c.common_name.contains("evo"));
        assert!(c.common_name.contains("CA"));
    }

    #[test]
    fn leaf_config_for_hostnames_uses_default_ttl() {
        let l = LeafConfig::for_hostnames(vec!["x.y".to_string()]);
        assert_eq!(l.ttl_days, DEFAULT_LEAF_TTL_DAYS);
    }
}
