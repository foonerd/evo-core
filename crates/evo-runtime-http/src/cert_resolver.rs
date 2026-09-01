// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Hot-reload cert resolver. Backs the live
//! `rustls::ServerConfig` with an `ArcSwap`-style cell so
//! the cert bundle can be replaced at runtime without
//! dropping in-flight connections.

use crate::error::RuntimeHttpError;
use crate::tls::build_server_config;
use evo_tls_certs::CertBundle;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use std::sync::{Arc, RwLock};

/// Resolver that holds the active server config behind an
/// `Arc<RwLock>` and looks up the certified key on every
/// client-hello.
///
/// New TLS connections pick up whatever bundle is current at
/// the moment the client-hello arrives; in-flight
/// connections continue with whatever they negotiated.
///
/// `swap` is cheap (one write lock acquire + one
/// CertifiedKey construction) and called rarely (cert
/// rotation), so the synchronous lock is fine.
///
/// When constructed via [`Self::new_with_client_verifier`] the
/// resolver also carries an optional
/// [`rustls::server::danger::ClientCertVerifier`]. The
/// verifier is consulted on every handshake; clients that
/// fail to present a cert chaining to one of the trusted
/// CAs are refused at the TLS layer before any HTTP request
/// is decoded. The verifier itself is immutable for the
/// resolver's lifetime — trust roots for client certs change
/// far less often than leaf certs and tying them to the same
/// rotation cadence would over-couple the two concerns.
#[derive(Debug)]
pub struct HotReloadCertResolver {
    inner: Arc<RwLock<Arc<CertifiedKey>>>,
    client_verifier:
        Option<Arc<dyn rustls::server::danger::ClientCertVerifier>>,
}

impl HotReloadCertResolver {
    /// Construct from an initial bundle, no client-cert
    /// verification. This is the framework-default posture —
    /// the server presents a cert; clients do not.
    pub fn new(bundle: &CertBundle) -> Result<Arc<Self>, RuntimeHttpError> {
        let key = certified_key_from_bundle(bundle)?;
        Ok(Arc::new(Self {
            inner: Arc::new(RwLock::new(Arc::new(key))),
            client_verifier: None,
        }))
    }

    /// Construct from an initial bundle PLUS a client-cert
    /// verifier. Every handshake is gated on the client
    /// presenting a cert that the verifier accepts; clients
    /// without a cert or with an untrusted one are refused
    /// at the TLS layer.
    pub fn new_with_client_verifier(
        bundle: &CertBundle,
        client_verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
    ) -> Result<Arc<Self>, RuntimeHttpError> {
        let key = certified_key_from_bundle(bundle)?;
        Ok(Arc::new(Self {
            inner: Arc::new(RwLock::new(Arc::new(key))),
            client_verifier: Some(client_verifier),
        }))
    }

    /// Whether this resolver was constructed with a client
    /// verifier attached. Useful for tests + boot-time logging.
    pub fn requires_client_cert(&self) -> bool {
        self.client_verifier.is_some()
    }

    /// Replace the active bundle. Subsequent client-hellos
    /// will be answered with the new cert.
    pub fn swap(&self, bundle: &CertBundle) -> Result<(), RuntimeHttpError> {
        let key = certified_key_from_bundle(bundle)?;
        let mut guard = self
            .inner
            .write()
            .expect("hot-reload resolver lock poisoned");
        *guard = Arc::new(key);
        Ok(())
    }

    /// Build a `rustls::ServerConfig` whose cert resolver is
    /// this hot-reload resolver, so the listener picks up
    /// `swap` calls automatically. When a client verifier was
    /// attached at construction, the config requires + verifies
    /// client certs; otherwise client auth is disabled.
    pub fn into_server_config(self: Arc<Self>) -> Arc<ServerConfig> {
        let resolver: Arc<dyn ResolvesServerCert> = self.clone();
        let builder = ServerConfig::builder();
        let mut cfg = match self.client_verifier.as_ref() {
            Some(verifier) => builder
                .with_client_cert_verifier(Arc::clone(verifier))
                .with_cert_resolver(resolver),
            None => builder.with_no_client_auth().with_cert_resolver(resolver),
        };
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(cfg)
    }
}

impl ResolvesServerCert for HotReloadCertResolver {
    fn resolve(
        &self,
        _client_hello: ClientHello<'_>,
    ) -> Option<Arc<CertifiedKey>> {
        let guard = self.inner.read().ok()?;
        Some(Arc::clone(&guard))
    }
}

fn certified_key_from_bundle(
    bundle: &CertBundle,
) -> Result<CertifiedKey, RuntimeHttpError> {
    // Build a throwaway ServerConfig to validate the bundle
    // (PEM parse + cert/key match), then extract the
    // certified key by re-parsing. rustls does not expose
    // the internal CertifiedKey from a built ServerConfig,
    // so we parse twice. Cert rotation is rare; the cost is
    // irrelevant.
    let _ = build_server_config(bundle)?;
    let chain = parse_certs(bundle.chain.as_bytes())?;
    let key = parse_key(bundle.private_key.as_bytes())?;
    // Use the process-installed crypto provider's key
    // loader so the cert path stays consistent with the
    // handshake path (ring or aws-lc-rs depending on
    // feature). `build_server_config` above installed the
    // default provider before returning.
    let provider =
        rustls::crypto::CryptoProvider::get_default().ok_or_else(|| {
            RuntimeHttpError::InvalidCertBundle(
                "no crypto provider installed".into(),
            )
        })?;
    let signing_key = provider
        .key_provider
        .load_private_key(key)
        .map_err(|e| RuntimeHttpError::InvalidCertBundle(e.to_string()))?;
    Ok(CertifiedKey::new(chain, signing_key))
}

fn parse_certs(
    pem: &[u8],
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, RuntimeHttpError> {
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

fn parse_key(
    pem: &[u8],
) -> Result<rustls::pki_types::PrivateKeyDer<'static>, RuntimeHttpError> {
    let mut reader = std::io::Cursor::new(pem);
    for item in rustls_pemfile::read_all(&mut reader) {
        let item = item
            .map_err(|e| RuntimeHttpError::InvalidCertBundle(e.to_string()))?;
        use rustls::pki_types::PrivateKeyDer;
        match item {
            rustls_pemfile::Item::Pkcs1Key(k) => {
                return Ok(PrivateKeyDer::Pkcs1(k))
            }
            rustls_pemfile::Item::Pkcs8Key(k) => {
                return Ok(PrivateKeyDer::Pkcs8(k))
            }
            rustls_pemfile::Item::Sec1Key(k) => {
                return Ok(PrivateKeyDer::Sec1(k))
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
    fn resolver_builds_from_valid_bundle() {
        let r = HotReloadCertResolver::new(&bundle()).unwrap();
        let cfg = r.into_server_config();
        assert_eq!(cfg.alpn_protocols.len(), 2);
    }

    #[test]
    fn resolver_swap_replaces_active_bundle() {
        let a = bundle();
        let b = bundle();
        let r = HotReloadCertResolver::new(&a).unwrap();
        let before = Arc::clone(&r.inner.read().unwrap());
        r.swap(&b).unwrap();
        let after = Arc::clone(&r.inner.read().unwrap());
        // Distinct bundles produce distinct certified keys.
        assert!(!Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn resolver_rejects_malformed_bundle_on_swap() {
        let r = HotReloadCertResolver::new(&bundle()).unwrap();
        let bad = CertBundle::from_pem("not a key", "not a chain");
        let err = r.swap(&bad).unwrap_err();
        assert!(matches!(err, RuntimeHttpError::InvalidCertBundle(_)));
    }
}
