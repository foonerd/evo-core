// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Hybrid post-quantum key exchange.
//!
//! When the `hybrid-pq` feature is enabled, the runtime
//! mount installs the `aws-lc-rs` crypto provider and
//! prefers `X25519MLKEM768` in the TLS 1.3 key-exchange
//! group list. The leaf cert + signature scheme stay
//! classical (ed25519); only key exchange is hybrid. This
//! is the standard deployment shape against the
//! cryptographically-relevant quantum-computer threat: an
//! adversary that records traffic today cannot decrypt the
//! handshake later by breaking the classical KEM, because
//! the symmetric key is derived from both X25519 *and*
//! ML-KEM-768 secrets.
//!
//! With the feature disabled, the module presents the
//! same public surface but every entry point compiles to a
//! no-op — the substrate stays on `ring` and the dep
//! footprint stays minimal.

/// Stable identifier the substrate uses on the wire for
/// the hybrid key-exchange group. Surfaces in
/// observability and operator-readable error messages.
pub const HYBRID_KX_GROUP_NAME: &str = "X25519MLKEM768";

/// Whether the runtime mount was built with hybrid PQ
/// support compiled in.
pub fn pq_compiled_in() -> bool {
    cfg!(feature = "hybrid-pq")
}

/// Install the framework's preferred crypto provider as the
/// process default.
///
/// Called once at boot before any `rustls::ServerConfig` is
/// built. The provider determines which kx groups + signing
/// schemes the server advertises:
///
/// - With `hybrid-pq`: `aws-lc-rs`, with `prefer-post-quantum`
///   enabled at the rustls feature level so the default kx
///   list has `X25519MLKEM768` first.
/// - Without `hybrid-pq`: `ring`, classical kx only.
///
/// Idempotent: subsequent calls are no-ops because rustls
/// stores a single process-wide default.
pub fn install_crypto_provider() {
    #[cfg(feature = "hybrid-pq")]
    {
        // aws-lc-rs provider. Once installed, every
        // `ServerConfig::builder()` call picks up its
        // default kx_groups, which (thanks to
        // `prefer-post-quantum`) leads with
        // X25519MLKEM768.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
    #[cfg(not(feature = "hybrid-pq"))]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// The kx groups the server advertises on TLS 1.3
/// handshakes.
///
/// With `hybrid-pq` on: `[X25519MLKEM768, X25519, SECP256R1, SECP384R1]`
/// — hybrid first, classical fallback. With it off:
/// `[X25519, SECP256R1, SECP384R1]`.
///
/// Surfaces a list of human-readable names so observability
/// can render the negotiated handshake without depending on
/// the rustls internal type.
pub fn advertised_kx_groups() -> Vec<&'static str> {
    #[cfg(feature = "hybrid-pq")]
    {
        vec!["X25519MLKEM768", "X25519", "SECP256R1", "SECP384R1"]
    }
    #[cfg(not(feature = "hybrid-pq"))]
    {
        vec!["X25519", "SECP256R1", "SECP384R1"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_in_matches_feature() {
        // The runtime check matches the cargo feature gate.
        assert_eq!(pq_compiled_in(), cfg!(feature = "hybrid-pq"));
    }

    #[test]
    fn advertised_groups_lead_with_hybrid_when_enabled() {
        let groups = advertised_kx_groups();
        if pq_compiled_in() {
            assert_eq!(groups[0], HYBRID_KX_GROUP_NAME);
        } else {
            assert!(!groups.contains(&HYBRID_KX_GROUP_NAME));
        }
    }

    #[test]
    fn install_crypto_provider_is_idempotent() {
        install_crypto_provider();
        install_crypto_provider();
        // No panic; subsequent installs are no-ops.
    }

    #[test]
    fn hybrid_kx_group_name_matches_rfc_designation() {
        // The IETF / NIST hybrid name; tooling on the
        // operator side matches against this exact string.
        assert_eq!(HYBRID_KX_GROUP_NAME, "X25519MLKEM768");
    }
}
