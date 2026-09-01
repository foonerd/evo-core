// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Cert bundle shapes: [`CertBundle`], [`CertChain`],
//! [`PrivateKey`], [`PemBytes`].

use serde::{Deserialize, Serialize};

/// One PEM-encoded byte string. Stored as a `String` for
/// trivial PEM file I/O; the wrapping type is here so the
/// surrounding code is clear about which strings carry
/// PEM-formatted bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PemBytes(String);

impl PemBytes {
    /// Construct from a PEM-formatted string.
    pub fn new(pem: impl Into<String>) -> Self {
        Self(pem.into())
    }

    /// Borrow as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the raw `String`.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Borrow as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// One private key in PEM form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrivateKey(PemBytes);

impl PrivateKey {
    /// Construct from a PEM-formatted key string.
    pub fn from_pem(pem: impl Into<String>) -> Self {
        Self(PemBytes::new(pem))
    }

    /// Borrow as a `&str` PEM block.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Borrow as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// A cert chain in PEM form. First entry is the leaf; each
/// subsequent entry is an intermediate issuer (root CA
/// optional — TLS endpoints typically include the
/// intermediate chain but not the root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertChain {
    /// PEM-formatted cert chain: leaf first, then issuers.
    pub chain: PemBytes,
}

impl CertChain {
    /// Construct from a PEM-formatted chain string. The
    /// caller is responsible for ordering (leaf first).
    pub fn from_pem(pem: impl Into<String>) -> Self {
        Self {
            chain: PemBytes::new(pem),
        }
    }

    /// Borrow as a `&str` PEM block.
    pub fn as_str(&self) -> &str {
        self.chain.as_str()
    }

    /// Borrow as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.chain.as_bytes()
    }
}

/// One TLS cert bundle ready for the runtime HTTPS
/// termination: private key + cert chain.
///
/// Issued by [`crate::device_ca::generate_leaf`] (device-CA
/// mode), loaded from disk by [`crate::manual::load_manual_bundle`]
/// (manual cert mode), or hot-swapped by the ACME runtime
/// (when the network-side renewal completes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertBundle {
    /// Private key for the leaf cert.
    pub private_key: PrivateKey,

    /// Cert chain (leaf first, then issuers).
    pub chain: CertChain,
}

impl CertBundle {
    /// Construct from raw PEM strings.
    pub fn from_pem(
        private_key_pem: impl Into<String>,
        chain_pem: impl Into<String>,
    ) -> Self {
        Self {
            private_key: PrivateKey::from_pem(private_key_pem),
            chain: CertChain::from_pem(chain_pem),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_bytes_round_trip() {
        let p = PemBytes::new("-----BEGIN-----\nabc\n-----END-----");
        assert!(p.as_str().contains("BEGIN"));
        assert_eq!(p.as_bytes(), b"-----BEGIN-----\nabc\n-----END-----");
    }

    #[test]
    fn private_key_construction() {
        let k = PrivateKey::from_pem("PRIVKEY");
        assert_eq!(k.as_str(), "PRIVKEY");
    }

    #[test]
    fn cert_chain_construction() {
        let c = CertChain::from_pem("CHAIN");
        assert_eq!(c.as_str(), "CHAIN");
    }

    #[test]
    fn cert_bundle_construction() {
        let b = CertBundle::from_pem("PRIVKEY", "CHAIN");
        assert_eq!(b.private_key.as_str(), "PRIVKEY");
        assert_eq!(b.chain.as_str(), "CHAIN");
    }

    #[test]
    fn bundle_round_trips_through_serde() {
        let b = CertBundle::from_pem("PRIVKEY", "CHAIN");
        let json = serde_json::to_string(&b).unwrap();
        let back: CertBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
    }
}
