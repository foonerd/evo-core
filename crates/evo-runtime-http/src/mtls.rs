// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Optional mTLS (mutual TLS) — client-certificate verification.
//!
//! When the operator supplies a CA bundle via
//! `EVO_MTLS_CLIENT_CA_FILE`, the HTTPS substrate refuses any
//! handshake whose client did not present a certificate
//! issued under (or chained to) one of the trusted CAs. This
//! lets fleet operators issue per-device or per-administrator
//! client certs and gate the management surface to those
//! credentials in addition to the resident bearer token.
//!
//! The verifier is built once at boot from the configured CA
//! bundle and attached to the live `HotReloadCertResolver`;
//! cert rotation (via `HotReloadCertResolver::swap`) does not
//! change the verifier — the trust root for client certs is
//! independent of the leaf cert rotation cadence.

use crate::error::RuntimeHttpError;
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;
use std::path::Path;
use std::sync::Arc;

/// Build a rustls `ClientCertVerifier` from a PEM file of one
/// or more trusted CA certificates. Each CERTIFICATE block in
/// the file is added to a fresh `RootCertStore`; the resulting
/// verifier accepts any client cert that chains to one of the
/// trusted roots.
///
/// Returns the verifier behind an `Arc` ready for the resolver
/// to attach. Failure modes:
///
/// - File I/O failure → [`RuntimeHttpError::InvalidCertBundle`].
/// - PEM parse failure → [`RuntimeHttpError::InvalidCertBundle`].
/// - The file contains zero CA certs →
///   [`RuntimeHttpError::InvalidCertBundle`].
pub fn build_client_verifier(
    client_ca_pem_path: &Path,
) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>, RuntimeHttpError>
{
    let pem = std::fs::read(client_ca_pem_path).map_err(|e| {
        RuntimeHttpError::InvalidCertBundle(format!(
            "read client-CA file {}: {}",
            client_ca_pem_path.display(),
            e
        ))
    })?;
    build_client_verifier_from_pem(&pem)
}

/// Variant of [`build_client_verifier`] that consumes the PEM
/// bytes directly. Useful for tests and for callers that have
/// the bundle in memory rather than on disk.
pub fn build_client_verifier_from_pem(
    pem: &[u8],
) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>, RuntimeHttpError>
{
    crate::pq::install_crypto_provider();

    let mut store = RootCertStore::empty();
    let mut count = 0usize;
    let mut reader = std::io::Cursor::new(pem);
    for item in rustls_pemfile::certs(&mut reader) {
        let der: CertificateDer<'static> = item.map_err(|e| {
            RuntimeHttpError::InvalidCertBundle(format!(
                "client-CA PEM parse: {e}"
            ))
        })?;
        store.add(der).map_err(|e| {
            RuntimeHttpError::InvalidCertBundle(format!(
                "client-CA add to root store: {e}"
            ))
        })?;
        count += 1;
    }
    if count == 0 {
        return Err(RuntimeHttpError::InvalidCertBundle(
            "client-CA PEM contains no CERTIFICATE blocks".into(),
        ));
    }

    WebPkiClientVerifier::builder(Arc::new(store))
        .build()
        .map_err(|e| {
            RuntimeHttpError::InvalidCertBundle(format!(
                "client verifier build: {e}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_tls_certs::{generate_device_ca, DeviceCaConfig};

    fn ca_pem() -> String {
        let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
        ca.ca_cert_pem
    }

    #[test]
    fn builds_from_valid_single_ca_pem() {
        let pem = ca_pem();
        let v = build_client_verifier_from_pem(pem.as_bytes())
            .expect("verifier builds");
        // Smoke: verifier offers no debug info we can introspect on
        // 0.23; the success path is "Ok return" plus no panic
        // installing the crypto provider. Drop is the only
        // observable cleanup; ensure that works without panic.
        drop(v);
    }

    #[test]
    fn builds_from_concatenated_multi_ca_pem() {
        let mut pem = ca_pem();
        pem.push_str(&ca_pem()); // duplicate is harmless
        let v = build_client_verifier_from_pem(pem.as_bytes())
            .expect("verifier builds");
        drop(v);
    }

    #[test]
    fn refuses_empty_pem() {
        let err = build_client_verifier_from_pem(b"").unwrap_err();
        match err {
            RuntimeHttpError::InvalidCertBundle(msg) => {
                assert!(msg.contains("no CERTIFICATE blocks"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn refuses_unreadable_path() {
        let nowhere =
            std::path::PathBuf::from("/tmp/evo-mtls-not-a-real-file-46f3e7");
        let err = build_client_verifier(&nowhere).unwrap_err();
        assert!(matches!(err, RuntimeHttpError::InvalidCertBundle(_)));
    }
}
