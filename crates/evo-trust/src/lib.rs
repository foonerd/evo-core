//! Trust root loading, install digests, ed25519 plugin signatures, and
//! revocations. See `docs/engineering/PLUGIN_PACKAGING.md` section 5.
//!
//! Scope split: this crate determines the effective
//! [`evo_plugin_sdk::manifest::TrustClass`] the steward should apply to
//! an out-of-process plugin based on its signature, its signing key's
//! authorisation, and its install digest. It does not spawn the plugin
//! and does not touch OS identity. Per-class UID/GID for out-of-process
//! spawns lives behind the opt-in `[plugins.security]` config in
//! `crates/evo/src/admission.rs` (see `PLUGIN_PACKAGING.md` section 5).
//! Seccomp, capabilities, namespaces, and any deeper sandboxing remain
//! distribution-owned; the reference path for Linux distributions is in
//! `PLUGIN_PACKAGING.md` section 5.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod canonical;
mod digest;
mod error;
mod key_meta;
mod matchers;
mod release_root;
mod revocation;
mod trust_root;
mod verify;

pub use canonical::{canonicalise, CanonicalError};
pub use digest::{
    format_digest_sha256_hex, install_digest, parse_digest_sha256_hex,
    signing_message, SIGNING_PAYLOAD_VERSION_V1,
};
pub use error::TrustError;
pub use key_meta::{Authorisation, KeyMeta, KeyRole, KeySection};
pub use matchers::effective_trust_class;
pub use matchers::name_matches_prefixes;
pub use release_root::{
    role_for_artefact_kind, verify_release_signature, ReleaseKeyMeta,
    ReleaseKeySection, ReleaseRole, ReleaseTrustKey,
};
pub use revocation::RevocationSet;
pub use trust_root::{load_trust_root, read_signature_file, TrustKey};
pub use verify::{
    verify_out_of_process_bundle, verify_out_of_process_bundle_at,
    OutOfProcessBundleRef, TrustOptions, TrustOutcome, MIN_ROTATION_OVERLAP,
};

use evo_plugin_sdk::manifest::TrustClass;

/// Minimum effective trust class required for a plugin to be admitted
/// with `capabilities.admin = true`.
///
/// Plugins whose effective trust class is strictly lower trust than
/// this constant (that is, `effective > ADMIN_MINIMUM_TRUST` in the
/// `Ord` on [`TrustClass`] where lower ordinal = more privileged)
/// are refused admission by the steward when they declare
/// `capabilities.admin = true`.
///
/// The constant lives in `evo-trust` so the admin policy sits
/// alongside the trust-class vocabulary, not diffused into the
/// admission engine. A distribution that wants to tighten or loosen
/// the threshold patches this file and rebuilds.
///
/// Currently set to [`TrustClass::Privileged`]. `Platform`
/// (compiled-in) auto-qualifies by being more privileged.
pub const ADMIN_MINIMUM_TRUST: TrustClass = TrustClass::Privileged;
