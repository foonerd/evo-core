//! Trust root loading, install digests, ed25519 plugin signatures, and
//! revocations. See `docs/engineering/PLUGIN_PACKAGING.md` section 5.
//!
//! OS-level trust class enforcement (gap [12]) is out of this crate: this
//! library only determines the effective [`TrustClass`] the steward should
//! apply; the process still spawns with the same user until [12] lands.

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
