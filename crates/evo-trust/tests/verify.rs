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
    verify_out_of_process_bundle, OutOfProcessBundleRef, RevocationSet,
    TrustError, TrustOptions,
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
