//! Out-of-process bundle checks: signature, key authorisation, revocations.
//!
//! ## Layered chain of trust
//!
//! A bundle's `manifest.sig` is verified against one of the loaded
//! [`TrustKey`]s. The verifier then walks the parent chain via
//! [`crate::KeySection::signed_by`] up to a self-declared root
//! (`signed_by = None`). At every step:
//!
//! - The parent referenced by `signed_by` must be present in the
//!   trust set; missing parents fail the chain.
//! - The current key's validity window
//!   (`not_before`/`not_after`) must contain the verification
//!   timestamp.
//! - The parent's role must be allowed to sign the child's role
//!   (e.g. a vendor cannot be signed by an individual author).
//!
//! Roots stand on their own: an [`crate::KeyRole::OperatorRoot`]
//! key may be a root itself (`signed_by = None`), declaring
//! supreme authority on the appliance.
//!
//! ## Rotation
//!
//! When a key carries `supersedes = Some(old_key_id)`, the verifier
//! also accepts a signature from the old key during the overlap
//! window: the period between the new key's `not_before` and the
//! old key's `not_after`. The minimum overlap is hard-coded to 30
//! days for v1. After overlap (= old key's `not_after`), the old
//! key is rejected with [`TrustError::KeyExpired`].
//!
//! ## Operator countersign / override
//!
//! When the trust set contains an [`crate::KeyRole::OperatorRoot`]
//! key and that key signs (or countersigns) the bundle, the
//! operator signature is preferred over a vendor signature. An
//! operator can also publish a key with `signed_by = None` and
//! `role = operator_root`, declaring supreme authority on this
//! appliance.

use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use ed25519_dalek::Verifier;
use evo_plugin_sdk::manifest::TrustClass;

use crate::digest::install_digest;
use crate::error::TrustError;
use crate::key_meta::KeyRole;
use crate::matchers::{effective_trust_class, name_matches_prefixes};
use crate::revocation::RevocationSet;
use crate::trust_root::{read_signature_file, TrustKey};

/// Minimum overlap window between a superseded key's `not_after`
/// and the new key's `not_before`. Hard-coded for v1; future
/// versions may make this configurable.
pub const MIN_ROTATION_OVERLAP: chrono::Duration = chrono::Duration::days(30);

/// Maximum chain depth before the verifier declares a chain
/// pathologically deep and bails out. Five tiers (project_root →
/// distribution_root → operator_root → vendor → individual_author)
/// covers every real shape; depth 8 leaves headroom and bounds the
/// walk against an adversarial trust root that loops back on itself.
const MAX_CHAIN_DEPTH: usize = 8;

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
/// The verification timestamp is `SystemTime::now()`.
pub fn verify_out_of_process_bundle(
    bundle: &OutOfProcessBundleRef<'_>,
    keys: &[TrustKey],
    revocations: &RevocationSet,
    opt: TrustOptions,
) -> Result<TrustOutcome, TrustError> {
    verify_out_of_process_bundle_at(
        bundle,
        keys,
        revocations,
        opt,
        SystemTime::now(),
    )
}

/// Same as [`verify_out_of_process_bundle`] but with an explicit
/// verification timestamp. Tests use this to drive the validity
/// window and rotation logic deterministically.
pub fn verify_out_of_process_bundle_at(
    bundle: &OutOfProcessBundleRef<'_>,
    keys: &[TrustKey],
    revocations: &RevocationSet,
    opt: TrustOptions,
    now: SystemTime,
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

    let now_dt: DateTime<Utc> = now.into();
    let msg =
        crate::digest::signing_message(bundle.manifest_path, bundle.exec_path)?;
    let sig = read_signature_file(bundle.plugin_dir)?;

    // Pass 1: collect every key whose ed25519 verify accepts the
    // signature. The first matching key is the "leaf"; multiple
    // accepting keys is unusual but valid (operator countersign).
    let mut signers: Vec<&TrustKey> = Vec::new();
    for k in keys {
        if k.verifying.verify(&msg, &sig).is_ok() {
            signers.push(k);
        }
    }
    if signers.is_empty() {
        return Err(TrustError::SignatureNotRecognised);
    }

    // Operator-supreme override: an operator_root signature wins
    // outright. The walk for an operator root is trivial (it is
    // already a root). Pick the first one if present.
    if let Some(op) = signers
        .iter()
        .find(|k| matches_role(k, KeyRole::OperatorRoot))
    {
        check_window(op, now_dt)?;
        return finalise(op, bundle, opt);
    }

    // Otherwise, walk each candidate signer's chain. The first one
    // whose chain is intact authorises the bundle.
    let mut last_err: Option<TrustError> = None;
    for leaf in &signers {
        match validate_chain(leaf, keys, now_dt) {
            Ok(()) => {
                if !name_matches_prefixes(
                    bundle.plugin_name,
                    &leaf.meta.authorisation.name_prefixes,
                ) {
                    last_err = Some(TrustError::NameNotAuthorised {
                        name: bundle.plugin_name.to_string(),
                        key_basename: leaf.basename.clone(),
                    });
                    continue;
                }
                return finalise(leaf, bundle, opt);
            }
            Err(e) => {
                // If a rotation predecessor of this leaf would
                // accept the bundle within the overlap window,
                // honour that. Otherwise propagate this leaf's
                // failure as the "best" error to surface.
                if let Some(rot) = rotation_window_accepts(leaf, keys, now_dt) {
                    if !name_matches_prefixes(
                        bundle.plugin_name,
                        &rot.meta.authorisation.name_prefixes,
                    ) {
                        last_err = Some(TrustError::NameNotAuthorised {
                            name: bundle.plugin_name.to_string(),
                            key_basename: rot.basename.clone(),
                        });
                        continue;
                    }
                    return finalise(rot, bundle, opt);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or(TrustError::SignatureNotRecognised))
}

/// True when the key's declared role equals `role`, treating an
/// absent role as the default [`KeyRole::Vendor`].
fn matches_role(k: &TrustKey, role: KeyRole) -> bool {
    k.meta.key.role.unwrap_or_default() == role
}

/// Check the validity window of `k` at `now`. `None` bounds are
/// open. Closed window outside the bounds returns
/// [`TrustError::KeyExpired`].
fn check_window(k: &TrustKey, now: DateTime<Utc>) -> Result<(), TrustError> {
    if let Some(nb) = k.meta.key.not_before {
        if now < nb {
            return Err(TrustError::KeyExpired {
                key_id: k.key_id.clone(),
                at: format!("not_before={nb}"),
            });
        }
    }
    if let Some(na) = k.meta.key.not_after {
        if now > na {
            return Err(TrustError::KeyExpired {
                key_id: k.key_id.clone(),
                at: format!("not_after={na}"),
            });
        }
    }
    Ok(())
}

/// Walk parent chain from `leaf` upward via `signed_by` → `key_id`,
/// validating window and role at each step, terminating at a root
/// (`signed_by = None`). Bounded by [`MAX_CHAIN_DEPTH`].
fn validate_chain(
    leaf: &TrustKey,
    all: &[TrustKey],
    now: DateTime<Utc>,
) -> Result<(), TrustError> {
    let mut cur: &TrustKey = leaf;
    let mut depth = 0usize;
    loop {
        check_window(cur, now)?;
        let parent_id = match cur.meta.key.signed_by.as_deref() {
            Some(p) => p,
            None => return Ok(()),
        };
        depth += 1;
        if depth > MAX_CHAIN_DEPTH {
            return Err(TrustError::ChainBroken {
                detail: format!(
                    "exceeded MAX_CHAIN_DEPTH ({MAX_CHAIN_DEPTH}) at {}",
                    cur.key_id
                ),
            });
        }
        let parent = all.iter().find(|p| p.key_id == parent_id).ok_or_else(
            || TrustError::ChainBroken {
                detail: format!(
                    "{} declares signed_by={parent_id} but no such key in trust set",
                    cur.key_id
                ),
            },
        )?;
        let cur_role = cur.meta.key.role.unwrap_or_default();
        let par_role = parent.meta.key.role.unwrap_or_default();
        if !parent_can_sign_child(par_role, cur_role) {
            return Err(TrustError::RoleMismatch {
                child: cur.key_id.clone(),
                parent: parent.key_id.clone(),
                child_role: cur_role,
                parent_role: par_role,
            });
        }
        cur = parent;
    }
}

/// Allowed parent roles per child role. The vocabulary is
/// deliberately simple: the chain is a partial order on roles, and
/// a key may only be signed by a role at least as authoritative as
/// itself.
fn parent_can_sign_child(parent: KeyRole, child: KeyRole) -> bool {
    use KeyRole::*;
    match child {
        ProjectRoot => false, // a project root has no parent
        DistributionRoot => matches!(parent, ProjectRoot),
        OperatorRoot => true, // operator root may also be a root itself
        Vendor => {
            matches!(parent, ProjectRoot | DistributionRoot | OperatorRoot)
        }
        IndividualAuthor => matches!(
            parent,
            ProjectRoot | DistributionRoot | OperatorRoot | Vendor
        ),
    }
}

/// If the trust set contains a key that supersedes `leaf` (or that
/// `leaf` supersedes), and the verification timestamp is inside the
/// overlap window, return that key. The returned key's chain is
/// validated against `now` so the rotation does not silently admit
/// a key whose own parent has expired.
fn rotation_window_accepts<'a>(
    leaf: &TrustKey,
    all: &'a [TrustKey],
    now: DateTime<Utc>,
) -> Option<&'a TrustKey> {
    // `leaf` is the OLD key, and a NEW key declares
    // `supersedes = leaf.key_id`. The overlap window runs from the
    // new key's `not_before` until the old key's `not_after`. The
    // case where `leaf` is the NEW key requires no action: the
    // caller's chain walk already accepted it.
    all.iter().find(|cand| {
        cand.meta.key.supersedes.as_deref() == Some(leaf.key_id.as_str())
            && in_overlap_window(leaf, cand, now)
            && validate_chain(cand, all, now).is_ok()
    })
}

/// True when `now` lies inside the overlap window between
/// `new.not_before` and `old.not_after`. Both bounds must be
/// declared; an open-ended bound is treated as no overlap so the
/// rotation contract is explicit.
fn in_overlap_window(
    old: &TrustKey,
    new: &TrustKey,
    now: DateTime<Utc>,
) -> bool {
    let (Some(nb), Some(na)) =
        (new.meta.key.not_before, old.meta.key.not_after)
    else {
        return false;
    };
    if nb > na {
        return false; // no overlap at all
    }
    if (na - nb) < MIN_ROTATION_OVERLAP {
        // Operator declared an overlap shorter than the minimum
        // policy window. Refuse rather than silently honouring it;
        // a too-short overlap is an operator misconfiguration.
        return false;
    }
    now >= nb && now <= na
}

fn finalise(
    k: &TrustKey,
    bundle: &OutOfProcessBundleRef<'_>,
    opt: TrustOptions,
) -> Result<TrustOutcome, TrustError> {
    if !name_matches_prefixes(
        bundle.plugin_name,
        &k.meta.authorisation.name_prefixes,
    ) {
        return Err(TrustError::NameNotAuthorised {
            name: bundle.plugin_name.to_string(),
            key_basename: k.basename.clone(),
        });
    }
    let key_max = k.meta.authorisation.max_trust_class;
    match effective_trust_class(
        bundle.declared_trust,
        key_max,
        opt.degrade_trust,
    ) {
        Ok(eff) => Ok(TrustOutcome {
            effective_trust: eff,
            was_unsigned: false,
        }),
        Err((d, m)) => Err(TrustError::TrustClassNotAuthorised {
            declared: d,
            key: k.basename.clone(),
            max: m,
        }),
    }
}
