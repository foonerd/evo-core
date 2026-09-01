// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`CertLifecycle`] — the observatory-aware wrapper over
//! the cert-substrate's free functions.
//!
//! The free functions ([`crate::generate_device_ca`],
//! [`crate::generate_leaf`],
//! [`crate::load_manual_bundle`]) remain pure compute. The
//! lifecycle wraps them and, when an [`Observatory`] is
//! attached, emits a structured observation at each
//! load-bearing seam (CA generation, leaf issuance, manual
//! bundle load, bundle rotation).

use crate::bundle::CertBundle;
use crate::device_ca::{
    generate_device_ca, generate_leaf, DeviceCaConfig, GeneratedCa, LeafConfig,
};
use crate::error::CertError;
use crate::manual::load_manual_bundle;
use evo_observatory::{
    Attributes, Observation, ObservationKind, Observatory, Outcome, SpanContext,
};
use std::path::Path;
use std::sync::Arc;

/// Substrate-aware wrapper over the cert lifecycle.
///
/// Construct one alongside an [`Observatory`] at boot; route
/// every cert event through it so the substrate's view of
/// "what TLS material exists, when, why" is captured live.
#[derive(Default)]
pub struct CertLifecycle {
    observatory: Option<Arc<Observatory>>,
}

impl CertLifecycle {
    /// Construct with no observatory — emissions are
    /// silent.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with an observatory attached.
    pub fn with_observatory(observatory: Arc<Observatory>) -> Self {
        Self {
            observatory: Some(observatory),
        }
    }

    /// Generate a fresh device-CA root.
    pub fn generate_device_ca(
        &self,
        config: &DeviceCaConfig,
    ) -> Result<GeneratedCa, CertError> {
        let ca = generate_device_ca(config)?;
        self.emit(
            Observation::now(
                SpanContext::new_root(),
                ObservationKind::TlsCaGenerated,
                Outcome::Success,
            )
            .with_attrs(
                Attributes::new()
                    .with("common_name", config.common_name.clone())
                    .with("ttl_days", config.ttl_days as u64)
                    .with("ca_cert_pem_bytes", ca.ca_cert_pem.len()),
            ),
        );
        Ok(ca)
    }

    /// Issue a leaf cert under the supplied device-CA.
    pub fn issue_leaf(
        &self,
        ca: &GeneratedCa,
        config: &LeafConfig,
    ) -> Result<CertBundle, CertError> {
        let bundle = ca.issue_leaf(config)?;
        self.emit(
            Observation::now(
                SpanContext::new_root(),
                ObservationKind::TlsLeafIssued,
                Outcome::Success,
            )
            .with_attrs(
                Attributes::new()
                    .with("hostnames", config.hostnames.clone())
                    .with("ttl_days", config.ttl_days as u64)
                    .with("chain_bytes", bundle.chain.as_str().len())
                    .with(
                        "private_key_bytes",
                        bundle.private_key.as_str().len(),
                    ),
            ),
        );
        Ok(bundle)
    }

    /// Generate both a device-CA and one leaf cert in a
    /// single call. Each emission seam fires independently
    /// so the operator sees the full chain.
    pub fn generate_device_ca_and_leaf(
        &self,
        ca_config: &DeviceCaConfig,
        leaf_config: &LeafConfig,
    ) -> Result<(GeneratedCa, CertBundle), CertError> {
        let (ca, bundle) = generate_leaf(ca_config, leaf_config)?;
        self.emit(
            Observation::now(
                SpanContext::new_root(),
                ObservationKind::TlsCaGenerated,
                Outcome::Success,
            )
            .with_attrs(
                Attributes::new()
                    .with("common_name", ca_config.common_name.clone())
                    .with("ttl_days", ca_config.ttl_days as u64),
            ),
        );
        self.emit(
            Observation::now(
                SpanContext::new_root(),
                ObservationKind::TlsLeafIssued,
                Outcome::Success,
            )
            .with_attrs(
                Attributes::new()
                    .with("hostnames", leaf_config.hostnames.clone())
                    .with("ttl_days", leaf_config.ttl_days as u64)
                    .with("chain_bytes", bundle.chain.as_str().len()),
            ),
        );
        Ok((ca, bundle))
    }

    /// Load a manual cert bundle from disk.
    pub fn load_manual_bundle(
        &self,
        key_path: impl AsRef<Path>,
        chain_path: impl AsRef<Path>,
    ) -> Result<CertBundle, CertError> {
        let key_path = key_path.as_ref();
        let chain_path = chain_path.as_ref();
        let bundle = load_manual_bundle(key_path, chain_path)?;
        self.emit(
            Observation::now(
                SpanContext::new_root(),
                ObservationKind::TlsManualBundleLoaded,
                Outcome::Success,
            )
            .with_attrs(
                Attributes::new()
                    .with("key_path", key_path.display().to_string())
                    .with("chain_path", chain_path.display().to_string())
                    .with(
                        "chain_certs",
                        bundle
                            .chain
                            .as_str()
                            .matches("BEGIN CERTIFICATE")
                            .count() as u64,
                    ),
            ),
        );
        Ok(bundle)
    }

    /// Emit a rotation marker. The caller invokes this
    /// after swapping a live bundle (e.g. after ACME renewal
    /// completes); the substrate records the rotation so the
    /// operator can correlate the new fingerprint with the
    /// previous one.
    pub fn record_rotation(
        &self,
        old_chain_bytes: usize,
        new_chain_bytes: usize,
    ) {
        self.emit(
            Observation::now(
                SpanContext::new_root(),
                ObservationKind::TlsBundleRotated,
                Outcome::Success,
            )
            .with_attrs(
                Attributes::new()
                    .with("old_chain_bytes", old_chain_bytes)
                    .with("new_chain_bytes", new_chain_bytes),
            ),
        );
    }

    fn emit(&self, obs: Observation) {
        if let Some(o) = &self.observatory {
            o.record(obs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_observatory::{Observatory, ObservatoryConfig};

    fn observed() -> (CertLifecycle, Arc<Observatory>) {
        let obs = Arc::new(Observatory::new(ObservatoryConfig::small()));
        let lifecycle = CertLifecycle::with_observatory(Arc::clone(&obs));
        (lifecycle, obs)
    }

    #[test]
    fn generate_device_ca_emits_observation() {
        let (l, observatory) = observed();
        l.generate_device_ca(&DeviceCaConfig::default()).unwrap();
        let snap = observatory.snapshot();
        assert!(snap
            .iter()
            .any(|o| o.kind == ObservationKind::TlsCaGenerated));
    }

    #[test]
    fn issue_leaf_emits_observation() {
        let (l, observatory) = observed();
        let ca = l.generate_device_ca(&DeviceCaConfig::default()).unwrap();
        l.issue_leaf(
            &ca,
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
        let snap = observatory.snapshot();
        assert!(snap
            .iter()
            .any(|o| o.kind == ObservationKind::TlsLeafIssued));
    }

    #[test]
    fn generate_device_ca_and_leaf_emits_both_kinds() {
        let (l, observatory) = observed();
        l.generate_device_ca_and_leaf(
            &DeviceCaConfig::default(),
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
        let snap = observatory.snapshot();
        let kinds: Vec<_> = snap.iter().map(|o| o.kind).collect();
        assert!(kinds.contains(&ObservationKind::TlsCaGenerated));
        assert!(kinds.contains(&ObservationKind::TlsLeafIssued));
    }

    #[test]
    fn record_rotation_emits_with_byte_counts() {
        let (l, observatory) = observed();
        l.record_rotation(1024, 1088);
        let snap = observatory.snapshot();
        let rotated = snap
            .iter()
            .find(|o| o.kind == ObservationKind::TlsBundleRotated)
            .unwrap();
        assert!(rotated.attrs.get("old_chain_bytes").is_some());
        assert!(rotated.attrs.get("new_chain_bytes").is_some());
    }

    #[test]
    fn lifecycle_without_observatory_is_silent() {
        let l = CertLifecycle::new();
        // Still functional; just doesn't emit.
        let ca = l.generate_device_ca(&DeviceCaConfig::default()).unwrap();
        l.issue_leaf(
            &ca,
            &LeafConfig::for_hostnames(vec!["device.local".to_string()]),
        )
        .unwrap();
    }
}
