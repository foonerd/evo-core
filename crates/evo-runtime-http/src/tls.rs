// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Build a `rustls::ServerConfig` from an `evo-tls-certs`
//! [`CertBundle`].

use crate::error::RuntimeHttpError;
use evo_tls_certs::CertBundle;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::sync::Arc;

/// Build a server-side rustls config from a cert bundle.
///
/// Parses the PEM-encoded chain and key, hands them to
/// rustls's `with_single_cert`, sets ALPN to negotiate
/// HTTP/2 then HTTP/1.1, and returns the config behind an
/// `Arc` ready for the listener to wrap.
///
/// Installs the framework's preferred crypto provider as a
/// side effect (idempotent, process-wide). With the
/// `hybrid-pq` feature on, this is the `aws-lc-rs` provider
/// and the default kx-group list leads with
/// `X25519MLKEM768`; otherwise `ring` with the classical
/// kx groups.
pub fn build_server_config(
    bundle: &CertBundle,
) -> Result<Arc<ServerConfig>, RuntimeHttpError> {
    crate::pq::install_crypto_provider();

    let chain = parse_certs(bundle.chain.as_bytes())?;
    let key = parse_key(bundle.private_key.as_bytes())?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| RuntimeHttpError::InvalidCertBundle(e.to_string()))?;

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

fn parse_certs(
    pem: &[u8],
) -> Result<Vec<CertificateDer<'static>>, RuntimeHttpError> {
    let mut reader = std::io::Cursor::new(pem);
    let mut out = Vec::new();
    for item in rustls_pemfile::certs(&mut reader) {
        let der = item
            .map_err(|e| RuntimeHttpError::InvalidCertBundle(e.to_string()))?;
        out.push(der);
    }
    if out.is_empty() {
        return Err(RuntimeHttpError::InvalidCertBundle(
            "chain contains no CERTIFICATE blocks".into(),
        ));
    }
    Ok(out)
}

fn parse_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, RuntimeHttpError> {
    let mut reader = std::io::Cursor::new(pem);
    for item in rustls_pemfile::read_all(&mut reader) {
        let item = item
            .map_err(|e| RuntimeHttpError::InvalidCertBundle(e.to_string()))?;
        match item {
            rustls_pemfile::Item::Pkcs1Key(k) => {
                return Ok(PrivateKeyDer::Pkcs1(k));
            }
            rustls_pemfile::Item::Pkcs8Key(k) => {
                return Ok(PrivateKeyDer::Pkcs8(k));
            }
            rustls_pemfile::Item::Sec1Key(k) => {
                return Ok(PrivateKeyDer::Sec1(k));
            }
            _ => {}
        }
    }
    Err(RuntimeHttpError::InvalidCertBundle(
        "no private key block found".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};

    fn bundle() -> CertBundle {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        ca.issue_leaf(&LeafConfig::for_hostnames(vec![
            "device.local".to_string()
        ]))
        .unwrap()
    }

    #[test]
    fn build_succeeds_from_fresh_device_ca_bundle() {
        let cfg = build_server_config(&bundle()).unwrap();
        assert_eq!(cfg.alpn_protocols.len(), 2);
        assert_eq!(cfg.alpn_protocols[0], b"h2".to_vec());
        assert_eq!(cfg.alpn_protocols[1], b"http/1.1".to_vec());
    }

    #[test]
    fn build_refuses_bundle_with_empty_chain() {
        let b = CertBundle::from_pem(bundle().private_key.as_str(), "");
        let err = build_server_config(&b).unwrap_err();
        assert!(matches!(err, RuntimeHttpError::InvalidCertBundle(_)));
    }

    #[test]
    fn build_refuses_bundle_with_empty_key() {
        let b = CertBundle::from_pem("", bundle().chain.as_str());
        let err = build_server_config(&b).unwrap_err();
        assert!(matches!(err, RuntimeHttpError::InvalidCertBundle(_)));
    }

    #[test]
    fn build_refuses_bundle_with_garbage_key() {
        let b = CertBundle::from_pem(
            "-----BEGIN CERTIFICATE-----\nXXXX\n-----END CERTIFICATE-----\n",
            bundle().chain.as_str(),
        );
        let err = build_server_config(&b).unwrap_err();
        assert!(matches!(err, RuntimeHttpError::InvalidCertBundle(_)));
    }
}
