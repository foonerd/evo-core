// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Manual cert mode: load an operator-supplied PEM bundle.

use crate::bundle::{CertBundle, CertChain, PrivateKey};
use crate::error::CertError;
use crate::pem_io::read_pem_file;
use std::path::Path;

/// Load a manual cert bundle from disk.
///
/// The bundle is two files: the private key (typically
/// `key.pem`) and the cert chain (typically `cert.pem` or
/// `fullchain.pem`, leaf first followed by issuer chain).
///
/// The framework validates the bundle shape by ensuring
/// the supplied PEM strings contain at least one
/// recognisable block via `rustls-pemfile`'s parser; deep
/// X.509 validation (signature / chain / hostname) happens
/// in the runtime TLS layer when rustls loads the bundle.
pub fn load_manual_bundle(
    key_path: impl AsRef<Path>,
    chain_path: impl AsRef<Path>,
) -> Result<CertBundle, CertError> {
    let key_pem = read_pem_file(key_path)?;
    let chain_pem = read_pem_file(chain_path)?;

    let key_pem = validate_private_key(&key_pem)?;
    let chain_pem = validate_chain(&chain_pem)?;

    Ok(CertBundle {
        private_key: PrivateKey::from_pem(key_pem),
        chain: CertChain::from_pem(chain_pem),
    })
}

fn validate_private_key(pem: &str) -> Result<String, CertError> {
    let mut reader = std::io::Cursor::new(pem.as_bytes());
    let mut found_any = false;
    for item in rustls_pemfile::read_all(&mut reader) {
        let item = item.map_err(|e| CertError::PemParse(e.to_string()))?;
        match item {
            rustls_pemfile::Item::Pkcs1Key(_)
            | rustls_pemfile::Item::Pkcs8Key(_)
            | rustls_pemfile::Item::Sec1Key(_) => {
                found_any = true;
            }
            _ => {}
        }
    }
    if !found_any {
        return Err(CertError::MissingBlock {
            missing: "private key (no PRIVATE KEY / RSA PRIVATE KEY / EC PRIVATE KEY block found)",
        });
    }
    Ok(pem.to_string())
}

fn validate_chain(pem: &str) -> Result<String, CertError> {
    let mut reader = std::io::Cursor::new(pem.as_bytes());
    let mut cert_count = 0;
    for item in rustls_pemfile::read_all(&mut reader) {
        let item = item.map_err(|e| CertError::PemParse(e.to_string()))?;
        if matches!(item, rustls_pemfile::Item::X509Certificate(_)) {
            cert_count += 1;
        }
    }
    if cert_count == 0 {
        return Err(CertError::MissingBlock {
            missing: "X.509 certificate (no CERTIFICATE block found)",
        });
    }
    Ok(pem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_ca::{generate_leaf, DeviceCaConfig, LeafConfig};
    use crate::pem_io::write_pem_file;
    use tempfile::tempdir;

    fn fixture_bundle_files(
    ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("key.pem");
        let chain_path = dir.path().join("chain.pem");
        let (_, bundle) = generate_leaf(
            &DeviceCaConfig::default(),
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
        write_pem_file(&key_path, bundle.private_key.as_str()).unwrap();
        write_pem_file(&chain_path, bundle.chain.as_str()).unwrap();
        (dir, key_path, chain_path)
    }

    #[test]
    fn load_round_trips_fresh_device_ca_bundle() {
        let (_dir, key, chain) = fixture_bundle_files();
        let loaded = load_manual_bundle(&key, &chain).unwrap();
        assert!(loaded.private_key.as_str().contains("PRIVATE KEY"));
        assert_eq!(
            loaded.chain.as_str().matches("BEGIN CERTIFICATE").count(),
            2
        );
    }

    #[test]
    fn load_refuses_missing_private_key_file() {
        let (_dir, _key, chain) = fixture_bundle_files();
        let result = load_manual_bundle("/no/such/key.pem", &chain);
        assert!(matches!(result, Err(CertError::Io { .. })));
    }

    #[test]
    fn load_refuses_missing_chain_file() {
        let (_dir, key, _chain) = fixture_bundle_files();
        let result = load_manual_bundle(&key, "/no/such/chain.pem");
        assert!(matches!(result, Err(CertError::Io { .. })));
    }

    #[test]
    fn load_refuses_key_pem_without_private_key_block() {
        let dir = tempdir().unwrap();
        let key = dir.path().join("not-a-key.pem");
        let chain = dir.path().join("chain.pem");
        write_pem_file(
            &key,
            "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let (_, bundle) = generate_leaf(
            &DeviceCaConfig::default(),
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
        write_pem_file(&chain, bundle.chain.as_str()).unwrap();
        let result = load_manual_bundle(&key, &chain);
        assert!(matches!(
            result,
            Err(CertError::MissingBlock { .. } | CertError::PemParse(_))
        ));
    }

    #[test]
    fn load_refuses_chain_pem_without_certificate_block() {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("key.pem");
        let chain_path = dir.path().join("empty.pem");
        let (_, bundle) = generate_leaf(
            &DeviceCaConfig::default(),
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
        write_pem_file(&key_path, bundle.private_key.as_str()).unwrap();
        write_pem_file(&chain_path, "# not a real chain\n").unwrap();
        let result = load_manual_bundle(&key_path, &chain_path);
        assert!(matches!(
            result,
            Err(CertError::MissingBlock { .. } | CertError::PemParse(_))
        ));
    }
}
