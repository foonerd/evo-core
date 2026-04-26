//! Trust-related errors.

use thiserror::Error;

/// Error from digest computation, file IO, or verification.
#[derive(Debug, Error)]
pub enum TrustError {
    /// I/O on manifest, artefact, or signature file.
    #[error("I/O {context}: {source}")]
    Io {
        /// Context string.
        context: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Missing or unreadable `manifest.sig`.
    #[error("manifest.sig is missing or not 64 bytes")]
    MissingOrBadSignature,
    /// No trust key in the root verified the signature.
    #[error(
        "ed25519 signature did not verify against any key in the trust root"
    )]
    SignatureNotRecognised,
    /// No `*.meta.toml` sidecar found for a `.pem` key, or TOML parse error.
    #[error("trust key sidecar: {0}")]
    KeyMetadata(String),
    /// The verifying key is not allowed to sign this plugin name.
    #[error("plugin name {name:?} is not authorised for key {key_basename}")]
    NameNotAuthorised {
        /// Installed name.
        name: String,
        /// Stem of the PEM file.
        key_basename: String,
    },
    /// Declared class cannot be satisfied and strict mode forbids degrading.
    #[error("trust: declared class {declared:?} is stronger than key {key} allows ({max:?})")]
    TrustClassNotAuthorised {
        /// What the manifest asks for.
        declared: evo_plugin_sdk::manifest::TrustClass,
        /// Human-readable key name.
        key: String,
        /// Maximum class that key may authorise.
        max: evo_plugin_sdk::manifest::TrustClass,
    },
    /// Install digest is listed in revocations.
    #[error("revoked: {0}")]
    Revoked(String),
    /// A signature was present but the operator has not enabled unsigned
    /// admission, and the manifest had no `manifest.sig` but should have.
    #[error("signed bundle required: manifest.sig is missing and allow_unsigned is false")]
    UnsignedInadmissible,
    /// A PEM or PKCS#8 public key could not be decoded.
    #[error("invalid public key: {0}")]
    BadPublicKey(String),
    /// The manifest could not be re-serialised to canonical TOML.
    /// Either the on-disk bytes did not parse as TOML, or a value
    /// (typically a non-finite float) cannot be represented
    /// canonically. The canonical TOML is the signing payload; no
    /// fallback to raw bytes is permitted.
    #[error("manifest canonicalisation failed: {0}")]
    CanonicalisationFailed(String),
}

impl TrustError {
    /// I/O with context.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
