//! Integration tests for `evo_trust::verify_out_of_process_bundle`.
//!
//! Exercises the full signature / authorisation / revocation pipeline
//! with real ed25519 keys and real on-disk bundles. The unit tests
//! inside the crate cover individual matchers and parsers; this file
//! covers the composition the steward invokes at admission.
//!
//! Keys are deterministic seeds (no RNG) so failures are reproducible.

use std::path::{Path, PathBuf};

use ed25519_dalek::pkcs8::EncodePublicKey;
use ed25519_dalek::{Signer, SigningKey};
use evo_plugin_sdk::manifest::TrustClass;
use evo_trust::{
    format_digest_sha256_hex, install_digest, load_trust_root, signing_message,
    verify_out_of_process_bundle, verify_out_of_process_bundle_at,
    OutOfProcessBundleRef, RevocationSet, TrustError, TrustOptions,
};
use pkcs8::LineEnding;

// Deterministic seeds; the exact bytes do not matter, only that they
// are distinct. Using 0xAA / 0xBB for readability in hex dumps.
const KEY_A_SEED: [u8; 32] = [0xAA; 32];
const KEY_B_SEED: [u8; 32] = [0xBB; 32];

fn sign_key(seed: [u8; 32]) -> SigningKey {
    SigningKey::from_bytes(&seed)
}

// A complete, valid manifest that `Manifest::from_toml` accepts and
// whose `plugin.name` is `org.example.echo`. Tests parameterise the
// `plugin_name` and `declared_trust` fields of `OutOfProcessBundleRef`
// explicitly and do not re-read the manifest body, so this fixed
// template is enough.
const MANIFEST_ECHO: &str = r#"[plugin]
name = "org.example.echo"
version = "0.1.0"
contract = 1

[target]
shelf = "example.echo"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "standard"

[prerequisites]
evo_min_version = "0.1.0"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
"#;

fn write_bundle(plugin_dir: &Path, artefact_bytes: &[u8]) {
    std::fs::write(plugin_dir.join("manifest.toml"), MANIFEST_ECHO).unwrap();
    std::fs::write(plugin_dir.join("plugin.bin"), artefact_bytes).unwrap();
}

fn write_signature(plugin_dir: &Path, key: &SigningKey) {
    let msg = signing_message(
        &plugin_dir.join("manifest.toml"),
        &plugin_dir.join("plugin.bin"),
    )
    .unwrap();
    let sig = key.sign(&msg);
    std::fs::write(plugin_dir.join("manifest.sig"), sig.to_bytes()).unwrap();
}

fn install_key(
    dir: &Path,
    stem: &str,
    key: &SigningKey,
    name_prefixes: &[&str],
    max_class: &str,
) {
    let pem = key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .unwrap();
    std::fs::write(dir.join(format!("{stem}.pem")), pem).unwrap();
    let prefixes = name_prefixes
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let meta = format!(
        "[authorisation]\nname_prefixes = [{prefixes}]\n\
         max_trust_class = \"{max_class}\"\n"
    );
    std::fs::write(dir.join(format!("{stem}.meta.toml")), meta).unwrap();
}

struct BundlePaths {
    plugin_dir: PathBuf,
    manifest_path: PathBuf,
    exec_path: PathBuf,
}

impl BundlePaths {
    fn for_dir(plugin_dir: &Path) -> Self {
        Self {
            plugin_dir: plugin_dir.to_path_buf(),
            manifest_path: plugin_dir.join("manifest.toml"),
            exec_path: plugin_dir.join("plugin.bin"),
        }
    }

    fn bundle<'a>(
        &'a self,
        plugin_name: &'a str,
        declared_trust: TrustClass,
    ) -> OutOfProcessBundleRef<'a> {
        OutOfProcessBundleRef {
            plugin_dir: &self.plugin_dir,
            manifest_path: &self.manifest_path,
            exec_path: &self.exec_path,
            plugin_name,
            declared_trust,
        }
    }
}

fn permissive_opts() -> TrustOptions {
    TrustOptions {
        allow_unsigned: false,
        degrade_trust: true,
    }
}

// -------------------------------------------------------------------
// Happy paths
// -------------------------------------------------------------------

#[test]
fn valid_signature_admits_at_declared_class() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);
    install_key(
        trust_opt.path(),
        "example",
        &key,
        &["org.example.*"],
        "platform",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    assert_eq!(keys.len(), 1);

    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    let outcome = verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    )
    .expect("valid signature should verify");
    assert_eq!(outcome.effective_trust, TrustClass::Standard);
    assert!(!outcome.was_unsigned);
}

#[test]
fn key_in_etc_trust_d_is_loaded_too() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);
    // Install into /etc/evo/trust.d equivalent, not /opt/evo/trust.
    install_key(
        trust_etc.path(),
        "operator",
        &key,
        &["org.example.*"],
        "platform",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    assert_eq!(keys.len(), 1);

    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);
    verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    )
    .expect("key loaded from trust_dir_etc should authorise");
}

// -------------------------------------------------------------------
// Authorisation failures
// -------------------------------------------------------------------

#[test]
fn name_not_authorised_by_any_key() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);
    // Key authorises a different namespace.
    install_key(
        trust_opt.path(),
        "vendor",
        &key,
        &["org.other.*"],
        "platform",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    ) {
        Err(TrustError::NameNotAuthorised { name, key_basename }) => {
            assert_eq!(name, "org.example.echo");
            assert_eq!(key_basename, "vendor");
        }
        other => panic!("expected NameNotAuthorised, got {other:?}"),
    }
}

#[test]
fn declared_class_above_key_max_degrades_when_enabled() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);
    // Key may authorise only up to `unprivileged`; manifest wants
    // `privileged`. With degrade_trust = true, admit at `unprivileged`.
    install_key(
        trust_opt.path(),
        "example",
        &key,
        &["org.example.*"],
        "unprivileged",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Privileged);

    let outcome = verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        TrustOptions {
            allow_unsigned: false,
            degrade_trust: true,
        },
    )
    .expect("degrade path should succeed");
    assert_eq!(outcome.effective_trust, TrustClass::Unprivileged);
    assert!(!outcome.was_unsigned);
}

#[test]
fn declared_class_above_key_max_refuses_in_strict_mode() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);
    install_key(
        trust_opt.path(),
        "example",
        &key,
        &["org.example.*"],
        "unprivileged",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Privileged);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        TrustOptions {
            allow_unsigned: false,
            degrade_trust: false,
        },
    ) {
        Err(TrustError::TrustClassNotAuthorised { declared, max, key }) => {
            assert_eq!(declared, TrustClass::Privileged);
            assert_eq!(max, TrustClass::Unprivileged);
            assert_eq!(key, "example");
        }
        other => panic!("expected TrustClassNotAuthorised, got {other:?}"),
    }
}

// -------------------------------------------------------------------
// Signature failures
// -------------------------------------------------------------------

#[test]
fn unrecognised_signature_refused() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let signer = sign_key(KEY_A_SEED);
    let other = sign_key(KEY_B_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    // Signed by A, but only B is installed in the trust root.
    write_signature(plugin_dir.path(), &signer);
    install_key(
        trust_opt.path(),
        "other",
        &other,
        &["org.example.*"],
        "platform",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    ) {
        Err(TrustError::SignatureNotRecognised) => {}
        other => panic!("expected SignatureNotRecognised, got {other:?}"),
    }
}

#[test]
fn signature_wrong_length_refused() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    // Write a bogus signature that is not 64 bytes.
    std::fs::write(
        plugin_dir.path().join("manifest.sig"),
        b"not-a-valid-signature",
    )
    .unwrap();
    install_key(
        trust_opt.path(),
        "example",
        &key,
        &["org.example.*"],
        "platform",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    ) {
        Err(TrustError::MissingOrBadSignature) => {}
        other => panic!("expected MissingOrBadSignature, got {other:?}"),
    }
}

// -------------------------------------------------------------------
// Unsigned admission
// -------------------------------------------------------------------

#[test]
fn missing_signature_refused_without_allow_unsigned() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    // No signature file.
    install_key(
        trust_opt.path(),
        "example",
        &key,
        &["org.example.*"],
        "platform",
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        TrustOptions {
            allow_unsigned: false,
            degrade_trust: true,
        },
    ) {
        Err(TrustError::UnsignedInadmissible) => {}
        other => panic!("expected UnsignedInadmissible, got {other:?}"),
    }
}

#[test]
fn missing_signature_admitted_at_sandbox_with_allow_unsigned() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    // No signature file; no keys installed either. The unsigned path
    // does not touch the trust root.
    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    assert!(keys.is_empty());

    let paths = BundlePaths::for_dir(plugin_dir.path());
    // Declared class is irrelevant on the unsigned path; the policy
    // forces `Sandbox` regardless.
    let bundle = paths.bundle("org.example.echo", TrustClass::Platform);

    let outcome = verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        TrustOptions {
            allow_unsigned: true,
            degrade_trust: true,
        },
    )
    .expect("unsigned + allow_unsigned should admit at sandbox");
    assert_eq!(outcome.effective_trust, TrustClass::Sandbox);
    assert!(outcome.was_unsigned);
}

// -------------------------------------------------------------------
// Revocation
// -------------------------------------------------------------------

#[test]
fn revoked_install_digest_refused_even_with_valid_signature() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let revocations_dir = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);
    install_key(
        trust_opt.path(),
        "example",
        &key,
        &["org.example.*"],
        "platform",
    );

    // Compute the install digest of our bundle and write it into a
    // revocations file. The RevocationSet loader parses the file.
    let id = install_digest(
        &plugin_dir.path().join("manifest.toml"),
        &plugin_dir.path().join("plugin.bin"),
    )
    .unwrap();
    let revs_path = revocations_dir.path().join("revocations.toml");
    let body = format!(
        "[[revoke]]\ndigest = \"{}\"\nreason = \"test\"\n",
        format_digest_sha256_hex(&id)
    );
    std::fs::write(&revs_path, body).unwrap();

    let revocations = RevocationSet::load(&revs_path).unwrap();
    assert!(revocations.is_revoked(&id));

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &revocations,
        permissive_opts(),
    ) {
        Err(TrustError::Revoked(display)) => {
            assert!(display.starts_with("sha256:"));
        }
        other => panic!("expected Revoked, got {other:?}"),
    }
}

#[test]
fn missing_revocations_file_is_empty_set() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("never-written.toml");
    let revocations = RevocationSet::load(&missing).unwrap();
    let any_digest = [0u8; 32];
    assert!(!revocations.is_revoked(&any_digest));
}

// -------------------------------------------------------------------
// Digest formula sanity
// -------------------------------------------------------------------

#[test]
fn install_digest_is_hash_of_signing_message() {
    // Per `PLUGIN_PACKAGING.md` section 4 (post-tightening rewording):
    // install_digest = SHA-256(signing_message) where
    // signing_message = manifest || SHA-256(artefact).
    // This test pins the formula so future refactors cannot silently
    // change it.
    use sha2::{Digest, Sha256};

    let plugin_dir = tempfile::tempdir().unwrap();
    write_bundle(plugin_dir.path(), b"artefact-bytes");
    let manifest_path = plugin_dir.path().join("manifest.toml");
    let exec_path = plugin_dir.path().join("plugin.bin");

    let msg = signing_message(&manifest_path, &exec_path).unwrap();
    let expected: [u8; 32] = Sha256::digest(&msg).into();
    let actual = install_digest(&manifest_path, &exec_path).unwrap();
    assert_eq!(actual, expected);
}

// -------------------------------------------------------------------
// Chain walk and rotation
// -------------------------------------------------------------------

const KEY_PARENT_SEED: [u8; 32] = [0xCC; 32];
const KEY_OPERATOR_SEED: [u8; 32] = [0xDD; 32];
const KEY_OLD_SEED: [u8; 32] = [0xEE; 32];
const KEY_NEW_SEED: [u8; 32] = [0xFF; 32];

/// Install a key with rich metadata. `key_id`, `signed_by`,
/// `supersedes`, `role`, and validity window are optional and
/// default to absent.
#[allow(clippy::too_many_arguments)]
fn install_key_full(
    dir: &Path,
    stem: &str,
    key: &SigningKey,
    name_prefixes: &[&str],
    max_class: &str,
    key_id: Option<&str>,
    signed_by: Option<&str>,
    supersedes: Option<&str>,
    role: Option<&str>,
    not_before: Option<&str>,
    not_after: Option<&str>,
) {
    let pem = key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .unwrap();
    std::fs::write(dir.join(format!("{stem}.pem")), pem).unwrap();
    let prefixes = name_prefixes
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut key_section = String::from("[key]\n");
    if let Some(v) = key_id {
        key_section.push_str(&format!("key_id = \"{v}\"\n"));
    }
    if let Some(v) = signed_by {
        key_section.push_str(&format!("signed_by = \"{v}\"\n"));
    }
    if let Some(v) = supersedes {
        key_section.push_str(&format!("supersedes = \"{v}\"\n"));
    }
    if let Some(v) = role {
        key_section.push_str(&format!("role = \"{v}\"\n"));
    }
    if let Some(v) = not_before {
        key_section.push_str(&format!("not_before = \"{v}\"\n"));
    }
    if let Some(v) = not_after {
        key_section.push_str(&format!("not_after = \"{v}\"\n"));
    }
    let meta = format!(
        "{key_section}\n[authorisation]\nname_prefixes = [{prefixes}]\n\
         max_trust_class = \"{max_class}\"\n"
    );
    std::fs::write(dir.join(format!("{stem}.meta.toml")), meta).unwrap();
}

fn at(rfc3339: &str) -> std::time::SystemTime {
    let dt = chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap();
    std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_secs(dt.timestamp() as u64)
}

#[test]
fn chain_walk_admits_when_parent_present_and_in_window() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let leaf = sign_key(KEY_A_SEED);
    let parent = sign_key(KEY_PARENT_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &leaf);

    install_key_full(
        trust_opt.path(),
        "vendor",
        &leaf,
        &["org.example.*"],
        "platform",
        Some("vendor_a"),
        Some("dist_root"),
        None,
        Some("vendor"),
        None,
        None,
    );
    install_key_full(
        trust_opt.path(),
        "dist",
        &parent,
        &["*"],
        "platform",
        Some("dist_root"),
        None,
        None,
        Some("distribution_root"),
        None,
        None,
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    )
    .expect("chain with present parent should admit");
}

#[test]
fn chain_walk_fails_when_parent_missing() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let leaf = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &leaf);

    // Vendor declares a parent that is not in the trust set.
    install_key_full(
        trust_opt.path(),
        "vendor",
        &leaf,
        &["org.example.*"],
        "platform",
        Some("vendor_a"),
        Some("missing_parent"),
        None,
        Some("vendor"),
        None,
        None,
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    ) {
        Err(TrustError::ChainBroken { detail }) => {
            assert!(detail.contains("missing_parent"));
        }
        other => panic!("expected ChainBroken, got {other:?}"),
    }
}

#[test]
fn key_outside_window_rejects_with_expired() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_A_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);
    install_key_full(
        trust_opt.path(),
        "expired",
        &key,
        &["org.example.*"],
        "platform",
        Some("vendor_a"),
        None,
        None,
        Some("vendor"),
        Some("2020-01-01T00:00:00Z"),
        Some("2021-01-01T00:00:00Z"),
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    let now = at("2030-01-01T00:00:00Z");
    match verify_out_of_process_bundle_at(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
        now,
    ) {
        Err(TrustError::KeyExpired { key_id, .. }) => {
            assert_eq!(key_id, "vendor_a");
        }
        other => panic!("expected KeyExpired, got {other:?}"),
    }
}

#[test]
fn rotation_overlap_admits_old_key_inside_window() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let old = sign_key(KEY_OLD_SEED);
    let new = sign_key(KEY_NEW_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    // Bundle is signed by the OLD key.
    write_signature(plugin_dir.path(), &old);

    // Old key valid 2026 .. 2027.
    install_key_full(
        trust_opt.path(),
        "old",
        &old,
        &["org.example.*"],
        "platform",
        Some("vendor_old"),
        None,
        None,
        Some("vendor"),
        Some("2026-01-01T00:00:00Z"),
        Some("2027-01-01T00:00:00Z"),
    );
    // New key supersedes old; valid 2026-06-01 .. 2028. The
    // overlap (old.not_after - new.not_before) = 2026-06-01..2027-01-01
    // = 7 months >= 30 days minimum.
    install_key_full(
        trust_opt.path(),
        "new",
        &new,
        &["org.example.*"],
        "platform",
        Some("vendor_new"),
        None,
        Some("vendor_old"),
        Some("vendor"),
        Some("2026-06-01T00:00:00Z"),
        Some("2028-01-01T00:00:00Z"),
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    // Verification timestamp inside the overlap window.
    let now = at("2026-09-01T00:00:00Z");
    verify_out_of_process_bundle_at(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
        now,
    )
    .expect("old key inside overlap should still admit");
}

#[test]
fn rotation_after_overlap_rejects_old_key() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let old = sign_key(KEY_OLD_SEED);
    let new = sign_key(KEY_NEW_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &old);

    install_key_full(
        trust_opt.path(),
        "old",
        &old,
        &["org.example.*"],
        "platform",
        Some("vendor_old"),
        None,
        None,
        Some("vendor"),
        Some("2026-01-01T00:00:00Z"),
        Some("2027-01-01T00:00:00Z"),
    );
    install_key_full(
        trust_opt.path(),
        "new",
        &new,
        &["org.example.*"],
        "platform",
        Some("vendor_new"),
        None,
        Some("vendor_old"),
        Some("vendor"),
        Some("2026-06-01T00:00:00Z"),
        Some("2028-01-01T00:00:00Z"),
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    // After the old key's not_after.
    let now = at("2027-06-01T00:00:00Z");
    match verify_out_of_process_bundle_at(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
        now,
    ) {
        Err(TrustError::KeyExpired { key_id, .. }) => {
            assert_eq!(key_id, "vendor_old");
        }
        other => panic!("expected KeyExpired, got {other:?}"),
    }
}

#[test]
fn operator_root_signature_is_preferred_when_present() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    // Single signing key serves both as a vendor and as the
    // operator-root in this fixture: the bundle's signature
    // verifies against both because the same private key is
    // installed twice. The verifier must prefer the operator
    // root entry.
    let key = sign_key(KEY_OPERATOR_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);

    install_key_full(
        trust_opt.path(),
        "vendor_dup",
        &key,
        &["org.example.*"],
        "standard",
        Some("vendor_a"),
        Some("dist_root"),
        None,
        Some("vendor"),
        None,
        None,
    );
    install_key_full(
        trust_etc.path(),
        "operator",
        &key,
        &["org.example.*"],
        "platform",
        Some("operator_root"),
        None,
        None,
        Some("operator_root"),
        None,
        None,
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Platform);

    // The vendor key is incomplete (its parent dist_root is not
    // present), but the operator root admits the bundle on its own.
    let outcome = verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    )
    .expect("operator root should admit even with broken vendor chain");
    assert_eq!(outcome.effective_trust, TrustClass::Platform);
}

// -------------------------------------------------------------------
// Operator-root override still validates the chain.
// -------------------------------------------------------------------

/// A key declares `role = operator_root` AND `signed_by =
/// <missing_parent>`. The override path used to short-circuit
/// chain validation; after the fix the chain walk runs and refuses
/// the bundle because the declared parent is absent from the trust
/// set. Closes the bypass where an operator-root-claimed key with
/// a fake parent could admit anything.
#[test]
fn operator_root_override_runs_chain_walk_and_rejects_missing_parent() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_OPERATOR_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);

    // operator_root with a `signed_by` that points at a key not
    // present in the trust set. A genuine operator root would have
    // `signed_by = None`; this fixture models the bypass attempt.
    install_key_full(
        trust_etc.path(),
        "rogue_operator",
        &key,
        &["org.example.*"],
        "platform",
        Some("operator_rogue"),
        Some("missing_vendor_parent"),
        None,
        Some("operator_root"),
        None,
        None,
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Platform);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    ) {
        Err(TrustError::ChainBroken { detail }) => {
            assert!(
                detail.contains("missing_vendor_parent"),
                "ChainBroken detail should name the missing parent, got {detail:?}"
            );
        }
        other => panic!("expected ChainBroken, got {other:?}"),
    }
}

/// Sanity: a genuine operator-root (`signed_by = None`) still
/// admits unchanged. The chain walk on a root is trivially
/// `Ok(())`.
#[test]
fn genuine_operator_root_still_admits_after_chain_walk_added() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let key = sign_key(KEY_OPERATOR_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &key);

    install_key_full(
        trust_etc.path(),
        "operator",
        &key,
        &["org.example.*"],
        "platform",
        Some("operator_root"),
        None, // genuine root: no signed_by
        None,
        Some("operator_root"),
        None,
        None,
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Platform);

    let outcome = verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    )
    .expect("genuine operator root should still admit");
    assert_eq!(outcome.effective_trust, TrustClass::Platform);
}

// -------------------------------------------------------------------
// Rotation override enforces intersection of authorisations.
// -------------------------------------------------------------------

/// OLD key authorises `org.foo.*` only; NEW key widens to
/// `org.foo.*` and `org.bar.*`. A bundle named `org.bar.something`
/// signed by OLD inside the overlap window must be REJECTED: the
/// OLD key's intent (what the operator originally signed off on)
/// did not include `org.bar.*`. Without the intersection check the
/// bundle would inherit NEW's broader prefix list.
#[test]
fn rotation_override_rejects_name_outside_old_keys_prefix_list() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let old = sign_key(KEY_OLD_SEED);
    let new = sign_key(KEY_NEW_SEED);
    let parent = sign_key(KEY_PARENT_SEED);

    // Bundle whose plugin.name is `org.example.echo` but whose
    // sidecar declares OLD's authorisation only covers
    // `org.example.*`. To exercise the bug we need a name covered
    // by NEW but NOT by OLD; install OLD and NEW with disjoint
    // prefix lists and use a manifest whose name matches NEW only.
    //
    // The shared MANIFEST_ECHO names `org.example.echo`, so put
    // OLD on `org.foo.*` and NEW on both `org.foo.*` and
    // `org.example.*`. The bundle's name (`org.example.echo`)
    // matches NEW but not OLD; the override path must refuse.
    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &old);

    // OLD valid 2026..2027, signed by parent so its chain is
    // inspectable; we expire the parent so OLD's chain fails and
    // the verifier consults the rotation predecessor path.
    install_key_full(
        trust_opt.path(),
        "old",
        &old,
        &["org.foo.*"],
        "platform",
        Some("vendor_old"),
        Some("expired_parent"),
        None,
        Some("vendor"),
        Some("2026-01-01T00:00:00Z"),
        Some("2027-01-01T00:00:00Z"),
    );
    install_key_full(
        trust_opt.path(),
        "expired_parent",
        &parent,
        &["*"],
        "platform",
        Some("expired_parent"),
        None,
        None,
        Some("distribution_root"),
        Some("2020-01-01T00:00:00Z"),
        Some("2020-06-01T00:00:00Z"),
    );
    // NEW supersedes OLD; valid 2026-06-01..2028; chain ends at a
    // healthy root via no `signed_by`.
    install_key_full(
        trust_opt.path(),
        "new",
        &new,
        &["org.foo.*", "org.example.*"],
        "platform",
        Some("vendor_new"),
        None,
        Some("vendor_old"),
        Some("operator_root"),
        Some("2026-06-01T00:00:00Z"),
        Some("2028-01-01T00:00:00Z"),
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    let now = at("2026-09-01T00:00:00Z");
    match verify_out_of_process_bundle_at(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
        now,
    ) {
        Err(TrustError::NameNotAuthorised { name, key_basename }) => {
            assert_eq!(name, "org.example.echo");
            assert_eq!(
                key_basename, "old",
                "the OLD key should be cited as the limiting authority"
            );
        }
        other => panic!(
            "expected NameNotAuthorised from old's prefix list, got {other:?}"
        ),
    }
}

/// OLD authorises up to `unprivileged` (less privileged ceiling);
/// NEW widens to `platform` (more privileged ceiling). A bundle
/// signed by OLD declaring `trust.class = platform` must be
/// REJECTED in strict mode: the intersection ceiling is OLD's
/// `unprivileged` cap, and `platform` is more privileged than
/// `unprivileged`.
#[test]
fn rotation_override_clamps_max_trust_class_to_old_keys_ceiling() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let old = sign_key(KEY_OLD_SEED);
    let new = sign_key(KEY_NEW_SEED);
    let parent = sign_key(KEY_PARENT_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    write_signature(plugin_dir.path(), &old);

    install_key_full(
        trust_opt.path(),
        "old",
        &old,
        &["org.example.*"],
        "unprivileged",
        Some("vendor_old"),
        Some("expired_parent"),
        None,
        Some("vendor"),
        Some("2026-01-01T00:00:00Z"),
        Some("2027-01-01T00:00:00Z"),
    );
    install_key_full(
        trust_opt.path(),
        "expired_parent",
        &parent,
        &["*"],
        "platform",
        Some("expired_parent"),
        None,
        None,
        Some("distribution_root"),
        Some("2020-01-01T00:00:00Z"),
        Some("2020-06-01T00:00:00Z"),
    );
    install_key_full(
        trust_opt.path(),
        "new",
        &new,
        &["org.example.*"],
        "platform",
        Some("vendor_new"),
        None,
        Some("vendor_old"),
        Some("operator_root"),
        Some("2026-06-01T00:00:00Z"),
        Some("2028-01-01T00:00:00Z"),
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    // Bundle declares `platform` (most privileged class).
    let bundle = paths.bundle("org.example.echo", TrustClass::Platform);

    let now = at("2026-09-01T00:00:00Z");
    match verify_out_of_process_bundle_at(
        &bundle,
        &keys,
        &RevocationSet::default(),
        TrustOptions {
            allow_unsigned: false,
            // Strict so the ceiling violation surfaces as an error
            // rather than degrading silently.
            degrade_trust: false,
        },
        now,
    ) {
        Err(TrustError::TrustClassNotAuthorised { declared, max, key }) => {
            assert_eq!(declared, TrustClass::Platform);
            assert_eq!(
                max,
                TrustClass::Unprivileged,
                "intersection ceiling is the OLD key's `unprivileged` cap"
            );
            assert_eq!(
                key, "old",
                "the OLD key should be cited as the limiting authority"
            );
        }
        other => panic!("expected TrustClassNotAuthorised, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// Trust chain edge-case coverage: cycle, role mismatch, malformed
// sidecar, mid-chain revocation.
// ------------------------------------------------------------------

const KEY_C_SEED: [u8; 32] = [0xC1; 32];
const KEY_D_SEED: [u8; 32] = [0xD1; 32];

/// A chain where every key declares `signed_by` with no
/// reachable root MUST bail with `ChainBroken` after at most
/// `MAX_CHAIN_DEPTH` (8) walked steps; the walker MUST NOT loop
/// indefinitely.
///
/// The fixture wires four keys into a cycle A → B → C → D → A. The
/// leaf key signing the bundle is one of them. The walker climbs
/// `signed_by` references and is bounded by `MAX_CHAIN_DEPTH`; the
/// cycle never reaches a `signed_by = None` root, so the depth
/// cap fires.
#[test]
fn chain_walk_cycle_terminates_at_max_chain_depth() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let a = sign_key(KEY_A_SEED);
    let b = sign_key(KEY_B_SEED);
    let c = sign_key(KEY_C_SEED);
    let d = sign_key(KEY_D_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    // Bundle is signed by A.
    write_signature(plugin_dir.path(), &a);

    // A → B → C → D → A: every key cites a parent and the cycle
    // never reaches a root.
    // All four keys are `operator_root` so the role pairing check
    // passes (operator_root may be signed by anything, including
    // operator_root) and only the cycle's depth-limit failure
    // remains as the surfacing condition.
    install_key_full(
        trust_opt.path(),
        "a",
        &a,
        &["org.example.*"],
        "platform",
        Some("a"),
        Some("b"),
        None,
        Some("operator_root"),
        None,
        None,
    );
    install_key_full(
        trust_opt.path(),
        "b",
        &b,
        &["*"],
        "platform",
        Some("b"),
        Some("c"),
        None,
        Some("operator_root"),
        None,
        None,
    );
    install_key_full(
        trust_opt.path(),
        "c",
        &c,
        &["*"],
        "platform",
        Some("c"),
        Some("d"),
        None,
        Some("operator_root"),
        None,
        None,
    );
    install_key_full(
        trust_opt.path(),
        "d",
        &d,
        &["*"],
        "platform",
        Some("d"),
        Some("a"),
        None,
        Some("operator_root"),
        None,
        None,
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    // Use a tight wall-clock to guard against any infinite-loop
    // bug regressing past MAX_CHAIN_DEPTH: this whole test must
    // complete promptly. If the bound is removed, we fail-fast
    // here on the timer rather than hanging the test runner.
    let start = std::time::Instant::now();
    let res = verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "chain walk took too long; depth cap may not be enforced"
    );
    match res {
        Err(TrustError::ChainBroken { detail }) => {
            assert!(
                detail.contains("MAX_CHAIN_DEPTH")
                    || detail.contains("exceeded"),
                "ChainBroken message must name the depth cap, got: {detail}"
            );
        }
        other => panic!("expected ChainBroken from cycle, got {other:?}"),
    }
}

/// A child key of role `vendor` whose declared `signed_by`
/// names another `vendor` key MUST be rejected with `RoleMismatch`.
/// The fixture installs V1 (vendor, root) and V2 (vendor, signed_by
/// = V1). A bundle signed by V2 verifies the signature, but the
/// chain walk MUST refuse the role pairing.
#[test]
fn chain_walk_rejects_vendor_signing_vendor_with_role_mismatch() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let v1 = sign_key(KEY_A_SEED);
    let v2 = sign_key(KEY_B_SEED);

    write_bundle(plugin_dir.path(), b"artefact-bytes");
    // Bundle is signed by V2 (the child).
    write_signature(plugin_dir.path(), &v2);

    // V1 is a vendor with no parent (a malformed root for its role,
    // but parseable by the loader).
    install_key_full(
        trust_opt.path(),
        "v1",
        &v1,
        &["*"],
        "platform",
        Some("vendor_one"),
        None,
        None,
        Some("vendor"),
        None,
        None,
    );
    // V2 is a vendor signed_by V1 — vendor cannot sign vendor.
    install_key_full(
        trust_opt.path(),
        "v2",
        &v2,
        &["org.example.*"],
        "platform",
        Some("vendor_two"),
        Some("vendor_one"),
        None,
        Some("vendor"),
        None,
        None,
    );

    let keys = load_trust_root(trust_opt.path(), trust_etc.path()).unwrap();
    let paths = BundlePaths::for_dir(plugin_dir.path());
    let bundle = paths.bundle("org.example.echo", TrustClass::Standard);

    match verify_out_of_process_bundle(
        &bundle,
        &keys,
        &RevocationSet::default(),
        permissive_opts(),
    ) {
        Err(TrustError::RoleMismatch {
            child,
            parent,
            child_role,
            parent_role,
        }) => {
            assert_eq!(child, "vendor_two");
            assert_eq!(parent, "vendor_one");
            assert_eq!(format!("{child_role:?}"), "Vendor");
            assert_eq!(format!("{parent_role:?}"), "Vendor");
        }
        other => panic!("expected RoleMismatch, got {other:?}"),
    }
}

/// A `*.meta.toml` sidecar with a malformed `not_after`
/// timestamp MUST cause `load_trust_root` to fail loudly. The
/// returned error MUST name the offending file path so the
/// operator can find it. The valid sidecar in the same directory
/// MUST NOT be silently accepted (reject-on-doubt: any malformed
/// entry fails the whole load).
#[test]
fn load_trust_root_partial_load_fails_loud_naming_offending_file() {
    let trust_opt = tempfile::tempdir().unwrap();
    let trust_etc = tempfile::tempdir().unwrap();
    let good = sign_key(KEY_A_SEED);
    let bad = sign_key(KEY_B_SEED);

    // Good key with a clean meta sidecar.
    install_key_full(
        trust_opt.path(),
        "good",
        &good,
        &["org.example.*"],
        "platform",
        Some("good_key"),
        None,
        None,
        Some("vendor"),
        None,
        None,
    );

    // Bad key: write the PEM normally but a malformed sidecar
    // (`not_after` is not RFC3339).
    let pem = bad
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .unwrap();
    std::fs::write(trust_opt.path().join("bad.pem"), pem).unwrap();
    let bad_meta = "[key]\n\
        key_id = \"bad_key\"\n\
        role = \"vendor\"\n\
        not_after = \"definitely-not-a-timestamp\"\n\
        \n\
        [authorisation]\n\
        name_prefixes = [\"org.example.*\"]\n\
        max_trust_class = \"platform\"\n";
    std::fs::write(trust_opt.path().join("bad.meta.toml"), bad_meta).unwrap();

    match load_trust_root(trust_opt.path(), trust_etc.path()) {
        Err(TrustError::KeyMetadata(detail)) => {
            assert!(
                detail.contains("bad.meta.toml"),
                "error must name the offending file, got: {detail}"
            );
        }
        Ok(keys) => panic!(
            "load_trust_root must fail-loud on a malformed sidecar; \
             instead it accepted {} keys",
            keys.len()
        ),
        other => panic!(
            "expected KeyMetadata error naming bad.meta.toml, got {other:?}"
        ),
    }
}

/// Revoking a key in the middle of a chain MUST cause a bundle
/// signed by a leaf below the revoked key to fail verification.
///
/// The current revocation surface (`RevocationSet`) only carries
/// install digests, NOT key revocations. Implementing key
/// revocation requires a new revocation entry kind (or an extended
/// schema for the existing file). That work is out of scope here;
/// the test is recorded so the gap is visible, and is `#[ignore]`'d
/// so CI does not fail until the surface lands.
#[test]
#[ignore = "key-revocation surface not implemented; install-digest \
            revocation only (RevocationSet::digests). Re-enable when \
            a key-revocation entry kind is added."]
fn revoked_distribution_key_invalidates_descendant_chain() {
    // Placeholder body so the harness still compiles; when key
    // revocation lands the body becomes:
    //   1. Install root → distribution → vendor chain.
    //   2. Sign a bundle with vendor.
    //   3. Add `distribution.key_id` to the revocation set.
    //   4. Verify must fail with the appropriate error.
    panic!("key-revocation surface not implemented yet");
}
