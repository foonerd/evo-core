//! Out-of-process bundle checks: signature, key authorisation, revocations.

use std::path::Path;

use ed25519_dalek::Verifier;
use evo_plugin_sdk::manifest::TrustClass;

use crate::digest::install_digest;
use crate::error::TrustError;
use crate::matchers::{effective_trust_class, name_matches_prefixes};
use crate::revocation::RevocationSet;
use crate::trust_root::{read_signature_file, TrustKey};

/// Full admission check for a plugin directory on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustOptions {
    /// If `true`, unsigned plugins may be admitted (always at `Sandbox` class
    /// for out-of-process discovery per `PLUGIN_PACKAGING.md` §5).
    pub allow_unsigned: bool,
    /// If a signature is valid but the manifest asked for a class above the
    /// key's `max_trust_class`, map down to the key's max instead of
    /// refusing. If `false`, that situation is an error.
    pub degrade_trust: bool,
}

/// Outcome: effective trust class the steward should use for the plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustOutcome {
    /// The class to enforce (after unsigned/signature/degrade rules).
    pub effective_trust: TrustClass,
    /// `true` if the bundle had no `manifest.sig` and was allowed via
    /// `allow_unsigned` only.
    pub was_unsigned: bool,
}

/// Bundle paths and names for [`verify_out_of_process_bundle`].
pub struct OutOfProcessBundleRef<'a> {
    /// Plugin root directory.
    pub plugin_dir: &'a Path,
    /// `manifest.toml` path.
    pub manifest_path: &'a Path,
    /// Resolved path to the executable artefact.
    pub exec_path: &'a Path,
    /// `plugin.name` from the manifest.
    pub plugin_name: &'a str,
    /// Declared [`TrustClass`] from the manifest.
    pub declared_trust: TrustClass,
}

/// Verify `manifest.sig` (if present), key authorisation, and revocation list.
/// `manifest` is the already-parsed value; `exec_path` is the resolved
/// artefact path. For unsigned path, returns `TrustOutcome` with
/// `effective_trust = TrustClass::Sandbox` and `was_unsigned = true` when
/// `allow_unsigned` is set.
pub fn verify_out_of_process_bundle(
    bundle: &OutOfProcessBundleRef<'_>,
    keys: &[TrustKey],
    revocations: &RevocationSet,
    opt: TrustOptions,
) -> Result<TrustOutcome, TrustError> {
    let id = install_digest(bundle.manifest_path, bundle.exec_path)?;
    if revocations.is_revoked(&id) {
        return Err(TrustError::Revoked(
            RevocationSet::display_digest(&id).to_string(),
        ));
    }

    let sig_path = bundle.plugin_dir.join("manifest.sig");
    let has_sig = sig_path.is_file();

    if !has_sig {
        if !opt.allow_unsigned {
            return Err(TrustError::UnsignedInadmissible);
        }
        return Ok(TrustOutcome {
            effective_trust: TrustClass::Sandbox,
            was_unsigned: true,
        });
    }

    let msg =
        crate::digest::signing_message(bundle.manifest_path, bundle.exec_path)?;
    let sig = read_signature_file(bundle.plugin_dir)?;

    let mut last_auth_err: Option<TrustError> = None;
    for k in keys {
        if k.verifying.verify(&msg, &sig).is_err() {
            continue;
        }
        if !name_matches_prefixes(
            bundle.plugin_name,
            &k.meta.authorisation.name_prefixes,
        ) {
            last_auth_err = Some(TrustError::NameNotAuthorised {
                name: bundle.plugin_name.to_string(),
                key_basename: k.basename.clone(),
            });
            continue;
        }
        let key_max = k.meta.authorisation.max_trust_class;
        match effective_trust_class(
            bundle.declared_trust,
            key_max,
            opt.degrade_trust,
        ) {
            Ok(eff) => {
                return Ok(TrustOutcome {
                    effective_trust: eff,
                    was_unsigned: false,
                });
            }
            Err((d, m)) => {
                return Err(TrustError::TrustClassNotAuthorised {
                    declared: d,
                    key: k.basename.clone(),
                    max: m,
                });
            }
        }
    }
    if let Some(e) = last_auth_err {
        return Err(e);
    }
    Err(TrustError::SignatureNotRecognised)
}
