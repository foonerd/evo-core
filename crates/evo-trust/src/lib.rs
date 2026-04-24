//! Trust root loading, install digests, ed25519 plugin signatures, and
//! revocations. See `docs/engineering/PLUGIN_PACKAGING.md` section 5.
//!
//! Scope split for gap [12]: this crate determines the effective
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

mod digest;
mod error;
mod key_meta;
mod matchers;
mod revocation;
mod trust_root;
mod verify;

pub use digest::{
    format_digest_sha256_hex, install_digest, parse_digest_sha256_hex,
    signing_message,
};
pub use error::TrustError;
pub use key_meta::{Authorisation, KeyMeta, KeySection};
pub use matchers::effective_trust_class;
pub use matchers::name_matches_prefixes;
pub use revocation::RevocationSet;
pub use trust_root::{load_trust_root, read_signature_file, TrustKey};
pub use verify::{
    verify_out_of_process_bundle, OutOfProcessBundleRef, TrustOptions,
    TrustOutcome,
};
